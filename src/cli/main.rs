use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use mimeclip_common::common::ipc::{socket_path, Request, Response};

#[derive(Parser)]
#[command(name = "mimeclip", about = "MIME-aware clipboard history", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List clipboard history (newest first)
    List {
        #[arg(short, long, default_value = "50")]
        limit: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Decode all MIME payloads for an entry (base64 JSON)
    Decode { id: i64 },
    /// Delete an entry
    Delete { id: i64 },
    /// Restore an entry to the clipboard (re-offers all MIME types)
    Restore { id: i64 },
    /// Clear all history
    Clear,
    /// Check if the daemon is running
    Ping,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let request = match &cli.cmd {
        Cmd::List { limit, .. } => Request::List {
            limit: Some(*limit),
        },
        Cmd::Decode { id } => Request::Decode { id: *id },
        Cmd::Delete { id } => Request::Delete { id: *id },
        Cmd::Restore { id } => Request::Restore { id: *id },
        Cmd::Clear => Request::Clear,
        Cmd::Ping => Request::Ping,
    };

    let response = send_request(request)?;

    match (&cli.cmd, response) {
        (_, Response::Error { message }) => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        (_, Response::Pong) => println!("pong"),
        (_, Response::Ok) => {}

        (Cmd::List { json: true, .. }, Response::List { entries }) => {
            println!("{}", serde_json::to_string_pretty(&entries)?);
        }
        (Cmd::List { .. }, Response::List { entries }) => {
            for e in &entries {
                println!(
                    "{id:>6}  {kind:<5}  {ts}  {label}",
                    id = e.id,
                    kind = e.kind.label(),
                    ts = e.timestamp.format("%Y-%m-%d %H:%M:%S"),
                    label = e.label,
                );
            }
        }

        (Cmd::Decode { .. }, Response::Decode { payloads }) => {
            println!("{}", serde_json::to_string_pretty(&payloads)?);
        }

        _ => {}
    }

    Ok(())
}

fn connect_with_retry() -> Result<UnixStream> {
    let path = socket_path();
    const RETRIES: u32 = 5;
    const DELAY: std::time::Duration = std::time::Duration::from_millis(200);

    for attempt in 0..=RETRIES {
        match UnixStream::connect(&path) {
            Ok(stream) => return Ok(stream),
            Err(e)
                if attempt < RETRIES
                    && matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
            {
                std::thread::sleep(DELAY);
            }
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("cannot connect to mimeclipd at {}", path.display())));
            }
        }
    }
    unreachable!()
}

fn send_request(req: Request) -> Result<Response> {
    let stream = connect_with_retry()?;

    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);

    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;

    let mut response_line = String::new();
    reader
        .lines()
        .next()
        .context("daemon closed connection")?
        .context("read response")?
        .clone_into(&mut response_line);

    let response: Response = serde_json::from_str(&response_line)?;
    Ok(response)
}
