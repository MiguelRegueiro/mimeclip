use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use mimeclip_common::common::config::{config_path, format_size, parse_size, save, Config};
use mimeclip_common::common::ipc::{socket_path, Request, Response};

#[derive(Parser)]
#[command(name = "mimeclip", about = "MIME-aware clipboard history", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List clipboard history (most recently used first)
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
    /// Offer a screenshot as an image, a plain filesystem path, and a file URI
    Screenshot { path: std::path::PathBuf },
    /// Clear all history
    Clear,
    /// View or change persistent history limits
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
    /// Check if the daemon is running
    Ping,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the current persistent settings
    Show,
    /// Set one setting without prompting (useful in scripts)
    Set {
        #[command(subcommand)]
        setting: ConfigSetting,
    },
}

#[derive(Subcommand)]
enum ConfigSetting {
    /// Limit the combined stored payload size, e.g. 256MiB
    MaxHistorySize { size: String },
    /// Limit the number of stored entries
    MaxEntries { count: usize },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Cmd::Config { action } = &cli.cmd {
        return handle_config(action);
    }

    let request = match &cli.cmd {
        Cmd::List { limit, .. } => Request::List {
            limit: Some(*limit),
        },
        Cmd::Decode { id } => Request::Decode { id: *id },
        Cmd::Delete { id } => Request::Delete { id: *id },
        Cmd::Restore { id } => Request::Restore { id: *id },
        Cmd::Screenshot { path } => Request::OfferScreenshot {
            path: path.to_string_lossy().into_owned(),
        },
        Cmd::Clear => Request::Clear,
        Cmd::Ping => Request::Ping,
        Cmd::Config { .. } => unreachable!("configuration is handled before IPC dispatch"),
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
                    ts = e.created_at.format("%Y-%m-%d %H:%M:%S"),
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

fn handle_config(action: &Option<ConfigCmd>) -> Result<()> {
    match action {
        Some(ConfigCmd::Show) => print_config(),
        Some(ConfigCmd::Set { setting }) => {
            let mut config = Config::load()?;
            match setting {
                ConfigSetting::MaxHistorySize { size } => {
                    let bytes = parse_size(size)?;
                    config.max_history_size = format_size(bytes);
                }
                ConfigSetting::MaxEntries { count } => config.max_entries = *count,
            }
            save(&config)?;
            reload_config()?;
            print_config()
        }
        None => interactive_config(),
    }
}

fn print_config() -> Result<()> {
    let config = Config::load()?;
    let limits = config.limits()?;
    println!("History settings");
    println!(
        "  Maximum total history size: {}",
        format_size(limits.max_history_size)
    );
    println!("  Maximum entries: {}", limits.max_entries);
    println!("  Saved in: {}", config_path()?.display());
    Ok(())
}

fn interactive_config() -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!("interactive configuration needs a terminal; use `mimeclip config show` or `mimeclip config set ...`");
    }

    let mut config = Config::load()?;
    let limits = config.limits()?;
    println!("MimeClip history settings");
    println!("Press Enter to keep a value. Changes apply immediately.");
    let size = prompt(&format!(
        "Maximum total history size [{}]: ",
        format_size(limits.max_history_size)
    ))?;
    if !size.is_empty() {
        config.max_history_size = format_size(parse_size(&size)?);
    }
    let count = prompt(&format!("Maximum entries [{}]: ", limits.max_entries))?;
    if !count.is_empty() {
        config.max_entries = count
            .parse()
            .context("maximum entries must be a positive whole number")?;
    }
    config.limits()?;
    save(&config)?;
    reload_config()?;
    println!("Saved. Existing oldest entries were trimmed only if they exceeded the new limit.");
    print_config()
}

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_owned())
}

fn reload_config() -> Result<()> {
    match send_request(Request::ReloadConfig)? {
        Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("could not apply configuration: {message}"),
        _ => anyhow::bail!("daemon returned an unexpected response while applying configuration"),
    }
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
