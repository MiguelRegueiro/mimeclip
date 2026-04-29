mod restore;
mod server;
mod suppress;
mod watcher;

use anyhow::{Error, Result};
use clap::Parser;
use log::info;
use std::io::ErrorKind;
use std::sync::{Arc, Mutex};
use wayland_client::{backend::WaylandError, ConnectError, DispatchError};

use mimeclip_common::common::db::Database;
use mimeclip_common::common::ipc::{db_path, socket_path};
use suppress::SuppressState;

#[derive(Parser)]
#[command(
    name = "mimeclipd",
    about = "MIME-aware clipboard history daemon",
    version
)]
struct Cli {}

fn should_retry_watcher_error(err: &Error) -> bool {
    err.chain().any(|cause| {
        if let Some(connect_error) = cause.downcast_ref::<ConnectError>() {
            return matches!(connect_error, ConnectError::NoCompositor);
        }

        if let Some(dispatch_error) = cause.downcast_ref::<DispatchError>() {
            return matches!(
                dispatch_error,
                DispatchError::Backend(WaylandError::Io(io_error))
                    if is_retryable_wayland_io(io_error)
            );
        }

        matches!(
            cause.downcast_ref::<WaylandError>(),
            Some(WaylandError::Io(io_error))
                if is_retryable_wayland_io(io_error)
        )
    })
}

fn is_retryable_wayland_io(io_error: &std::io::Error) -> bool {
    matches!(
        io_error.kind(),
        ErrorKind::BrokenPipe
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::NotConnected
            | ErrorKind::UnexpectedEof
    )
}

fn main() -> Result<()> {
    let _cli = Cli::parse();

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let db_path = db_path();
    info!("database: {}", db_path.display());

    let db = Arc::new(Mutex::new(Database::open(&db_path)?));

    // Shared hash used to suppress re-storing entries that we just restored.
    let suppress_hash = Arc::new(Mutex::new(SuppressState::default()));

    // Clean up socket on SIGTERM or SIGINT.
    let sock = socket_path();
    std::thread::spawn(move || {
        use signal_hook::consts::{SIGINT, SIGTERM};
        use signal_hook::iterator::Signals;
        let mut signals = Signals::new([SIGTERM, SIGINT]).expect("signal handler");
        if signals.forever().next().is_some() {
            std::fs::remove_file(&sock).ok();
            std::process::exit(0);
        }
    });

    // Start IPC server in a background thread.
    let db_ipc = Arc::clone(&db);
    let suppress_ipc = Arc::clone(&suppress_hash);
    std::thread::spawn(move || {
        if let Err(e) = server::run(db_ipc, suppress_ipc) {
            log::error!("IPC server error: {e}");
        }
    });

    // Run clipboard watcher on main thread (needs to be on same thread as Wayland event loop).
    // Reconnects automatically if the Wayland connection drops (e.g. compositor restart,
    // seat reset, broken pipe). Fatal errors unrelated to Wayland still propagate up so
    // systemd can restart the whole process.
    loop {
        match watcher::run(Arc::clone(&db), Arc::clone(&suppress_hash)) {
            Ok(()) => return Ok(()),
            Err(e) if should_retry_watcher_error(&e) => {
                log::warn!("watcher unavailable ({e:#}), reconnecting in 2s");
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_retryable_wayland_io, should_retry_watcher_error};
    use anyhow::Error;
    use std::io;
    use wayland_client::{backend::WaylandError, ConnectError, DispatchError};

    #[test]
    fn retries_when_no_compositor_is_available() {
        let err = Error::new(ConnectError::NoCompositor);

        assert!(should_retry_watcher_error(&err));
    }

    #[test]
    fn retries_when_wayland_connection_drops() {
        let err = Error::new(DispatchError::from(WaylandError::Io(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "broken pipe",
        ))));

        assert!(should_retry_watcher_error(&err));
    }

    #[test]
    fn does_not_retry_fatal_startup_errors() {
        let err = Error::new(ConnectError::InvalidFd);

        assert!(!should_retry_watcher_error(&err));
    }

    #[test]
    fn does_not_retry_bad_protocol_messages() {
        let err = Error::new(DispatchError::BadMessage {
            sender_id: wayland_client::backend::ObjectId::null(),
            interface: "wl_display",
            opcode: 0,
        });

        assert!(!should_retry_watcher_error(&err));
    }

    #[test]
    fn does_not_retry_unrelated_io_errors() {
        let io_error = io::Error::new(io::ErrorKind::Other, "other io failure");
        let err = Error::new(DispatchError::from(WaylandError::Io(io_error)));

        assert!(!should_retry_watcher_error(&err));
    }

    #[test]
    fn classifies_disconnect_style_io_errors_as_retryable() {
        let io_error = io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe");

        assert!(is_retryable_wayland_io(&io_error));
    }
}
