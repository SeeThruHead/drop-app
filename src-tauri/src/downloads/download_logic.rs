use crate::auth::generate_authorization_header;
use crate::db::DatabaseImpls;
use crate::downloads::manifest::DropDownloadContext;
use crate::remote::RemoteAccessError;
use crate::DB;
use log::warn;
use md5::{Context, Digest};
use reqwest::blocking::Response;
use tauri::utils::acl::Permission;

use std::fs::{set_permissions, Permissions};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::thread::sleep;
use std::time::Duration;
use std::{
    fs::{File, OpenOptions},
    io::{self, BufWriter, Seek, SeekFrom, Write},
    path::PathBuf,
};
use urlencoding::encode;

use super::download_agent::GameDownloadError;
use super::download_thread_control_flag::{DownloadThreadControl, DownloadThreadControlFlag};
use super::progress_object::ProgressHandle;

pub struct DropWriter<W: Write> {
    hasher: Context,
    destination: W,
}
impl DropWriter<File> {
    fn new(path: PathBuf) -> Self {
        Self {
            destination: OpenOptions::new().write(true).open(path).unwrap(),
            hasher: Context::new(),
        }
    }

    fn finish(mut self) -> io::Result<Digest> {
        self.flush().unwrap();
        Ok(self.hasher.compute())
    }
}
// Write automatically pushes to file and hasher
impl Write for DropWriter<File> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        /*
        self.hasher.write_all(buf).map_err(|e| {
            io::Error::new(
                ErrorKind::Other,
                format!("Unable to write to hasher: {}", e),
            )
        })?;
         */
        self.destination.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // self.hasher.flush()?;
        self.destination.flush()
    }
}
// Seek moves around destination output
impl Seek for DropWriter<File> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.destination.seek(pos)
    }
}

pub struct DropDownloadPipeline<R: Read, W: Write> {
    pub source: R,
    pub destination: DropWriter<W>,
    pub control_flag: DownloadThreadControl,
    pub progress: ProgressHandle,
    pub size: usize,
}
impl DropDownloadPipeline<Response, File> {
    fn new(
        source: Response,
        destination: DropWriter<File>,
        control_flag: DownloadThreadControl,
        progress: ProgressHandle,
        size: usize,
    ) -> Self {
        Self {
            source,
            destination,
            control_flag,
            progress,
            size,
        }
    }

    fn copy(&mut self) -> Result<bool, io::Error> {
        let copy_buf_size = 512;
        let mut copy_buf = vec![0; copy_buf_size];
        let mut buf_writer = BufWriter::with_capacity(1024 * 1024, &mut self.destination);

        let mut current_size = 0;
        loop {
            if self.control_flag.get() == DownloadThreadControlFlag::Stop {
                return Ok(false);
            }

            let bytes_read = self.source.read(&mut copy_buf)?;
            current_size += bytes_read;

            buf_writer.write_all(&copy_buf[0..bytes_read])?;
            self.progress.add(bytes_read);

            if current_size == self.size {
                break;
            }
        }

        Ok(true)
    }

    fn finish(self) -> Result<Digest, io::Error> {
        let checksum = self.destination.finish()?;
        Ok(checksum)
    }
}

pub fn download_game_chunk(
    ctx: DropDownloadContext,
    control_flag: DownloadThreadControl,
    progress: ProgressHandle,
) -> Result<bool, GameDownloadError> {
    // If we're paused
    if control_flag.get() == DownloadThreadControlFlag::Stop {
        progress.set(0);
        return Ok(false);
    }

    let base_url = DB.fetch_base_url();

    let client = reqwest::blocking::Client::new();
    let chunk_url = base_url
        .join(&format!(
            "/api/v1/client/chunk?id={}&version={}&name={}&chunk={}",
            // Encode the parts we don't trust
            ctx.game_id,
            encode(&ctx.version),
            encode(&ctx.file_name),
            ctx.index
        ))
        .unwrap();

    let header = generate_authorization_header();

    let response = client
        .get(chunk_url)
        .header("Authorization", header)
        .send()
        .map_err(|e| GameDownloadError::Communication(e.into()))?;

    if response.status() != 200 {
        warn!("{}", response.text().unwrap());
        return Err(GameDownloadError::Communication(
            RemoteAccessError::InvalidCodeError(400),
        ));
    }

    let mut destination = DropWriter::new(ctx.path.clone());

    if ctx.offset != 0 {
        destination
            .seek(SeekFrom::Start(ctx.offset))
            .expect("Failed to seek to file offset");
    }

    let content_length = response.content_length();
    if content_length.is_none() {
        return Err(GameDownloadError::Communication(
            RemoteAccessError::InvalidResponse,
        ));
    }

    let mut pipeline = DropDownloadPipeline::new(
        response,
        destination,
        control_flag,
        progress,
        content_length.unwrap().try_into().unwrap(),
    );

    let completed = pipeline.copy().map_err(GameDownloadError::IoError)?;
    if !completed {
        return Ok(false);
    };

    // If we complete the file, set the permissions (if on Linux)
    #[cfg(unix)]
    {
        let permissions = Permissions::from_mode(ctx.permissions);
        set_permissions(ctx.path, permissions).unwrap();
    }

    /*
    let checksum = pipeline
        .finish()
        .map_err(|e| GameDownloadError::IoError(e))?;

    let res = hex::encode(checksum.0);
    if res != ctx.checksum {
        return Err(GameDownloadError::Checksum);
    }
     */

    Ok(true)
}
