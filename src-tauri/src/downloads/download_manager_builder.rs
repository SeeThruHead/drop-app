use std::{
    collections::HashMap,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread::spawn,
};

use log::{error, info, warn};
use tauri::{AppHandle, Emitter};

use crate::{db::DatabaseGameStatus, library::GameUpdateEvent, DB};

use super::{
    download_agent::{GameDownloadAgent, GameDownloadError},
    download_manager::{
        AgentInterfaceData, DownloadManager, DownloadManagerSignal, DownloadManagerStatus,
        GameDownloadStatus,
    },
    download_thread_control_flag::{DownloadThreadControl, DownloadThreadControlFlag},
    progress_object::ProgressObject,
    queue::Queue,
};

/*

Welcome to the download manager, the most overengineered, glorious piece of bullshit.

The download manager takes a queue of game_ids and their associated
GameDownloadAgents, and then, one-by-one, executes them. It provides an interface
to interact with the currently downloading agent, and manage the queue.

When the DownloadManager is initialised, it is designed to provide a reference
which can be used to provide some instructions (the DownloadManagerInterface),
but other than that, it runs without any sort of interruptions.

It does this by opening up two data structures. Primarily is the command_receiver,
and mpsc (multi-channel-single-producer) which allows commands to be sent from
the Interface, and queued up for the Manager to process.

These have been mapped in the DownloadManagerSignal docs.

The other way to interact with the DownloadManager is via the donwload_queue,
which is just a collection of ids which may be rearranged to suit
whichever download queue order is required.

+----------------------------------------------------------------------------+
| DO NOT ATTEMPT TO ADD OR REMOVE FROM THE QUEUE WITHOUT USING SIGNALS!!     |
| THIS WILL CAUSE A DESYNC BETWEEN THE DOWNLOAD AGENT REGISTRY AND THE QUEUE |
| WHICH HAS NOT BEEN ACCOUNTED FOR                                           |
+----------------------------------------------------------------------------+

This download queue does not actually own any of the GameDownloadAgents. It is
simply a id-based reference system. The actual Agents are stored in the
download_agent_registry HashMap, as ordering is no issue here. This is why
appending or removing from the download_queue must be done via signals.

Behold, my madness - quexeky

*/

pub struct DownloadManagerBuilder {
    download_agent_registry: HashMap<String, Arc<GameDownloadAgent>>,
    download_queue: Queue,
    command_receiver: Receiver<DownloadManagerSignal>,
    sender: Sender<DownloadManagerSignal>,
    progress: Arc<Mutex<Option<ProgressObject>>>,
    status: Arc<Mutex<DownloadManagerStatus>>,
    app_handle: AppHandle,

    current_game_interface: Option<Arc<AgentInterfaceData>>, // Should be the only game download agent in the map with the "Go" flag
    active_control_flag: Option<DownloadThreadControl>,
}

impl DownloadManagerBuilder {
    pub fn build(app_handle: AppHandle) -> DownloadManager {
        let queue = Queue::new();
        let (command_sender, command_receiver) = channel();
        let active_progress = Arc::new(Mutex::new(None));
        let status = Arc::new(Mutex::new(DownloadManagerStatus::Empty));

        let manager = Self {
            download_agent_registry: HashMap::new(),
            download_queue: queue.clone(),
            command_receiver,
            current_game_interface: None,
            active_control_flag: None,
            status: status.clone(),
            sender: command_sender.clone(),
            progress: active_progress.clone(),
            app_handle,
        };

        let terminator = spawn(|| manager.manage_queue());

        DownloadManager::new(terminator, queue, active_progress, command_sender)
    }

    fn set_game_status(&self, id: String, status: DatabaseGameStatus) {
        let mut db_handle = DB.borrow_data_mut().unwrap();
        db_handle
            .games
            .games_statuses
            .insert(id.clone(), status.clone());
        drop(db_handle);
        DB.save().unwrap();
        self.app_handle
            .emit(
                &format!("update_game/{}", id),
                GameUpdateEvent {
                    game_id: id,
                    status: status,
                },
            )
            .unwrap();
    }

    fn manage_queue(mut self) -> Result<(), ()> {
        loop {
            let signal = match self.command_receiver.recv() {
                Ok(signal) => signal,
                Err(_) => return Err(()),
            };

            match signal {
                DownloadManagerSignal::Go => {
                    self.manage_go_signal();
                }
                DownloadManagerSignal::Stop => {
                    self.manage_stop_signal();
                }
                DownloadManagerSignal::Completed(game_id) => {
                    self.manage_completed_signal(game_id);
                }
                DownloadManagerSignal::Queue(game_id, version, target_download_dir) => {
                    self.manage_queue_signal(game_id, version, target_download_dir);
                }
                DownloadManagerSignal::Finish => {
                    if let Some(active_control_flag) = self.active_control_flag {
                        active_control_flag.set(DownloadThreadControlFlag::Stop)
                    }
                    return Ok(());
                }
                DownloadManagerSignal::Error(e) => {
                    self.manage_error_signal(e);
                }
                DownloadManagerSignal::Cancel(id) => {
                    self.manage_cancel_signal(id);
                }
            };
        }
    }

    fn manage_stop_signal(&mut self) {
        info!("Got signal 'Stop'");
        if let Some(active_control_flag) = self.active_control_flag.clone() {
            active_control_flag.set(DownloadThreadControlFlag::Stop);
        }
    }

    fn manage_completed_signal(&mut self, game_id: String) {
        info!("Got signal 'Completed'");
        if let Some(interface) = &self.current_game_interface {
            // When if let chains are stabilised, combine these two statements
            if interface.id == game_id {
                info!("Popping consumed data");
                self.download_queue.pop_front();
                self.download_agent_registry.remove(&game_id);
                self.active_control_flag = None;
                *self.progress.lock().unwrap() = None;

                self.set_game_status(game_id, DatabaseGameStatus::Installed);
            }
        }
        self.sender.send(DownloadManagerSignal::Go).unwrap();
    }

    fn manage_queue_signal(&mut self, id: String, version: String, target_download_dir: usize) {
        info!("Got signal Queue");
        let download_agent = Arc::new(GameDownloadAgent::new(
            id.clone(),
            version,
            target_download_dir,
            self.sender.clone(),
        ));
        let agent_status = GameDownloadStatus::Uninitialised;
        let interface_data = AgentInterfaceData {
            id: id.clone(),
            status: Mutex::new(agent_status),
        };
        self.download_agent_registry
            .insert(interface_data.id.clone(), download_agent);
        self.download_queue.append(interface_data);

        self.set_game_status(id, DatabaseGameStatus::Queued);
    }

    fn manage_go_signal(&mut self) {
        info!("Got signal 'Go'");

        if !(!self.download_agent_registry.is_empty() && !self.download_queue.empty()) {
            return;
        }

        info!("Starting download agent");
        let agent_data = self.download_queue.read().front().unwrap().clone();
        let download_agent = self
            .download_agent_registry
            .get(&agent_data.id)
            .unwrap()
            .clone();
        self.current_game_interface = Some(agent_data);

        let progress_object = download_agent.progress.clone();
        *self.progress.lock().unwrap() = Some(progress_object);

        let active_control_flag = download_agent.control_flag.clone();
        self.active_control_flag = Some(active_control_flag.clone());

        let sender = self.sender.clone();

        info!("Spawning download");
        spawn(move || {
            match download_agent.download() {
                // Returns once we've exited the download
                // (not necessarily completed)
                // The download agent will fire the completed event for us
                Ok(_) => {}
                // If an error occurred while *starting* the download
                Err(err) => {
                    error!("error while managing download: {}", err);
                    sender.send(DownloadManagerSignal::Error(err)).unwrap();
                }
            };
        });

        active_control_flag.set(DownloadThreadControlFlag::Go);
        self.set_status(DownloadManagerStatus::Downloading);
        self.set_game_status(
            self.current_game_interface.as_ref().unwrap().id.clone(),
            DatabaseGameStatus::Downloading,
        );
    }
    fn manage_error_signal(&self, error: GameDownloadError) {
        let current_status = self.current_game_interface.clone().unwrap();
        let mut lock = current_status.status.lock().unwrap();
        *lock = GameDownloadStatus::Error;
        self.set_status(DownloadManagerStatus::Error(error));
    }
    fn manage_cancel_signal(&mut self, game_id: String) {
        if let Some(current_flag) = &self.active_control_flag {
            current_flag.set(DownloadThreadControlFlag::Stop);
            self.active_control_flag = None;
            *self.progress.lock().unwrap() = None;
        }
        // TODO wait until current download exits

        self.download_agent_registry.remove(&game_id);
        let mut lock = self.download_queue.edit();
        let index = match lock.iter().position(|interface| interface.id == game_id) {
            Some(index) => index,
            None => return,
        };
        lock.remove(index);

        // Start next download
        self.sender.send(DownloadManagerSignal::Go).unwrap();
        info!(
            "{:?}",
            self.download_agent_registry
                .iter()
                .map(|x| x.0.clone())
                .collect::<String>()
        );
    }
    fn set_status(&self, status: DownloadManagerStatus) {
        *self.status.lock().unwrap() = status;
    }
}
