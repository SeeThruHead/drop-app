use std::{
    borrow::{Borrow, BorrowMut},
    sync::Mutex,
};

use serde::Deserialize;
use url::Url;

use crate::{AppState, AppStatus, DB};

macro_rules! unwrap_or_return {
    ( $e:expr ) => {
        match $e {
            Ok(x) => x,
            Err(e) => {
                return Err(format!(
                    "Invalid URL or Drop is inaccessible ({})",
                    e.to_string()
                ))
            }
        }
    };
}

#[derive(Deserialize)]
struct DropHealthcheck {
    appName: String,
}

#[tauri::command]
pub async fn use_remote<'a>(
    url: String,
    state: tauri::State<'_, Mutex<AppState>>,
) -> Result<(), String> {
    println!("connecting to url {}", url);
    let base_url = unwrap_or_return!(Url::parse(&url));

    // Test Drop url
    let test_endpoint = base_url.join("/api/v1").unwrap();
    let response = unwrap_or_return!(reqwest::get(test_endpoint.to_string()).await);

    let result = response.json::<DropHealthcheck>().await.unwrap();

    if result.appName != "Drop" {
        return Err("Not a valid Drop endpoint".to_string());
    }

    let mut app_state = state.lock().unwrap();
    app_state.status = AppStatus::SignedOut;
    drop(app_state);

    let mut db_state = DB.borrow_data_mut().unwrap();
    db_state.base_url = base_url.to_string();
    drop(db_state);

    DB.save().unwrap();

    return Ok(());
}