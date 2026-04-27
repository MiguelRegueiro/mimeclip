mod restore;
mod server;
mod suppress;
mod watcher;

use anyhow::Result;
use clap::Parser;
use log::info;
use std::sync::{Arc, Mutex};

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
    watcher::run(db, suppress_hash)?;

    Ok(())
}
