use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use log::{debug, error, info, warn};

use mimeclip_common::common::db::Database;
use mimeclip_common::common::ipc::{socket_path, Request, Response};

use crate::restore;
use crate::suppress::SharedSuppressState;

const MAX_SCREENSHOT_BYTES: u64 = 64 * 1024 * 1024;

fn screenshot_payloads(path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("resolving screenshot path {}", path.display()))?;
    let metadata = std::fs::metadata(&path)?;
    if !metadata.is_file() {
        bail!("screenshot path is not a regular file: {}", path.display());
    }
    if metadata.len() > MAX_SCREENSHOT_BYTES {
        bail!(
            "screenshot is larger than the {} MiB limit",
            MAX_SCREENSHOT_BYTES / (1024 * 1024)
        );
    }

    let mime = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => bail!("unsupported screenshot image format: {}", path.display()),
    };

    let path_text = path.to_string_lossy().into_owned();
    let uri = format!("file://{}\r\n", file_uri_path(&path_text));
    let image = std::fs::read(&path)
        .with_context(|| format!("reading screenshot image {}", path.display()))?;

    Ok(vec![
        (mime.to_string(), image),
        (
            "text/plain;charset=utf-8".to_string(),
            path_text.into_bytes(),
        ),
        ("text/uri-list".to_string(), uri.into_bytes()),
    ])
}

fn file_uri_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

pub fn run(db: Arc<Mutex<Database>>, suppress_hash: SharedSuppressState) -> Result<()> {
    let path = socket_path();

    // Remove stale socket.
    if path.exists() {
        std::fs::remove_file(&path).ok();
    }

    let listener = UnixListener::bind(&path)?;
    info!("IPC socket: {}", path.display());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let db = Arc::clone(&db);
                let suppress = Arc::clone(&suppress_hash);
                std::thread::spawn(move || {
                    if let Err(e) = handle_client(stream, db, suppress) {
                        error!("client error: {e}");
                    }
                });
            }
            Err(e) => warn!("accept: {e}"),
        }
    }
    Ok(())
}

fn handle_client(
    stream: std::os::unix::net::UnixStream,
    db: Arc<Mutex<Database>>,
    suppress_hash: SharedSuppressState,
) -> Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        debug!("ipc request: {line}");

        let response = match serde_json::from_str::<Request>(&line) {
            Err(e) => Response::Error {
                message: format!("parse error: {e}"),
            },
            Ok(req) => dispatch(req, &db, &suppress_hash),
        };

        let mut out = serde_json::to_string(&response)?;
        out.push('\n');
        writer.write_all(out.as_bytes())?;
        writer.flush()?;
    }
    Ok(())
}

fn dispatch(
    req: Request,
    db: &Arc<Mutex<Database>>,
    suppress_hash: &SharedSuppressState,
) -> Response {
    match req {
        Request::Ping => Response::Pong,

        Request::ReloadConfig => match db.lock().unwrap().reload_config() {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },

        Request::List { limit } => {
            let limit = limit.unwrap_or(200);
            match db.lock().unwrap().list(limit) {
                Ok(entries) => Response::List { entries },
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }

        Request::Decode { id } => match db.lock().unwrap().get_payloads(id) {
            Ok(payloads) => Response::Decode { payloads },
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },

        Request::Delete { id } => match db.lock().unwrap().delete(id) {
            Ok(true) => Response::Ok,
            Ok(false) => Response::Error {
                message: format!("no entry with id {id}"),
            },
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },

        Request::OfferScreenshot { path } => match screenshot_payloads(Path::new(&path)) {
            Ok(payloads) => {
                std::thread::spawn(move || {
                    if let Err(e) = restore::restore_entry(payloads) {
                        error!("offering screenshot: {e}");
                    }
                });
                Response::Ok
            }
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },

        Request::Clear => match db.lock().unwrap().clear() {
            Ok(_) => Response::Ok,
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        },

        Request::Restore { id } => {
            let (hash, payloads) = {
                let db = db.lock().unwrap();
                let hash = match db.get_hash(id) {
                    Ok(Some(hash)) => hash,
                    Ok(None) => {
                        return Response::Error {
                            message: format!("no entry with id {id}"),
                        }
                    }
                    Err(e) => {
                        return Response::Error {
                            message: e.to_string(),
                        }
                    }
                };

                let payloads = match db.get_raw_payloads(id) {
                    Ok(payloads) => payloads,
                    Err(e) => {
                        return Response::Error {
                            message: e.to_string(),
                        }
                    }
                };

                if let Err(e) = db.touch_last_used(id) {
                    return Response::Error {
                        message: e.to_string(),
                    };
                }

                (hash, payloads)
            };

            suppress_hash.lock().unwrap().arm(hash);

            // Run restore in a new thread so IPC stays responsive.
            std::thread::spawn(move || {
                if let Err(e) = restore::restore_entry(payloads) {
                    error!("restore: {e}");
                }
            });

            Response::Ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::file_uri_path;

    #[test]
    fn file_uri_path_escapes_reserved_characters() {
        assert_eq!(
            file_uri_path("/tmp/a screenshot#1.png"),
            "/tmp/a%20screenshot%231.png"
        );
    }
}
