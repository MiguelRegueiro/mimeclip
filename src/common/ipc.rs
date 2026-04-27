use serde::{Deserialize, Serialize};

use super::types::{Entry, MimePayload};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    List { limit: Option<usize> },
    Decode { id: i64 },
    Delete { id: i64 },
    Restore { id: i64 },
    Clear,
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok,
    List { entries: Vec<Entry> },
    Decode { payloads: Vec<MimePayload> },
    Error { message: String },
    Pong,
}

pub fn socket_path() -> std::path::PathBuf {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(runtime).join("mimeclipd.sock")
}

pub fn db_path() -> std::path::PathBuf {
    let data = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            std::path::PathBuf::from(home).join(".local/share")
        });
    let dir = data.join("mimeclip");
    std::fs::create_dir_all(&dir).ok();
    dir.join("history.db")
}
