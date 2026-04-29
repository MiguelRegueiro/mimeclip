use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use log::{debug, error, info, warn};

use mimeclip_common::common::db::Database;
use mimeclip_common::common::ipc::{socket_path, Request, Response};

use crate::restore;
use crate::suppress::SharedSuppressState;

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
