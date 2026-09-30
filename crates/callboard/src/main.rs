mod board_cli;
mod view_cli;

use callboard::{
    Error, client,
    lifecycle::{Environment, Paths},
    server, setup,
};
use callboard_core::feed::{
    ChangeSummary, MAX_SNAPSHOT_BYTES, parse_submission, validate_feed_name,
};
use clap::{Parser, Subcommand};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
};

#[derive(Parser)]
#[command(version, about = "A per-user Linux bulletin board")]
struct Cli {
    /// Require an already running service.
    #[arg(long, global = true)]
    no_auto_start: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Run the service in the foreground (SIGINT/SIGTERM shuts down cleanly).
    Serve,
    /// Install and enable a systemd user service for future logins.
    Setup {
        /// Print the unit without installing it or calling systemctl.
        #[arg(long)]
        print: bool,
    },
    /// Submit a complete snapshot from a JSON array or object on stdin.
    Put {
        name: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        source_url: Option<String>,
        #[arg(long)]
        stale_after: Option<String>,
        /// Exit with this code if new items were added (takes precedence).
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..))]
        exit_added: Option<u8>,
        /// Exit with this code if items were added, removed, or updated.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..))]
        exit_changed: Option<u8>,
    },
    /// List boards as JSON.
    Boards,
    Board {
        #[command(subcommand)]
        command: board_cli::BoardCommand,
    },
    Todo {
        #[command(subcommand)]
        command: board_cli::TodoCommand,
    },
    Note {
        #[command(subcommand)]
        command: board_cli::NoteCommand,
    },
    /// Read the archive of items from deleted boards.
    Archive,
    /// List saved layouts as JSON, including their cards.
    Layouts,
    Layout {
        #[command(subcommand)]
        command: view_cli::LayoutCommand,
    },
    /// List feeds as JSON.
    Feeds,
    /// Read one feed as JSON.
    Get { name: String },
    /// Report a failed fetch without changing items.
    Fail { name: String, message: String },
    Feed {
        #[command(subcommand)]
        command: view_cli::FeedCommand,
    },
}
#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("callboard: {e}");
            ExitCode::FAILURE
        }
    }
}
async fn run(cli: Cli) -> Result<u8, Error> {
    let paths = Paths::resolve(&Environment::current())?;
    let executable: PathBuf = std::env::current_exe()?;
    let auto = (!cli.no_auto_start).then_some(executable.as_path());
    let (method, resource, body, exits) = match cli.command {
        Command::Serve => {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            server::serve(paths, async move {
                tokio::select! { _ = term.recv() => (), _ = interrupt.recv() => () }
            })
            .await?;
            return Ok(0);
        }
        Command::Setup { print } => {
            if print {
                print!("{}", setup::render(&paths, &executable)?);
            } else {
                let unit = setup::install(&paths, &executable).await?;
                println!(
                    "Installed and enabled {} for future logins. An already running service is left running.",
                    unit.display()
                );
            }
            return Ok(0);
        }
        Command::Put {
            name,
            title,
            source_url,
            stale_after,
            exit_added,
            exit_changed,
        } => {
            validate_feed_name(&name)?;
            // Read on the blocking pool, bounded before JSON allocation.
            let input = tokio::task::spawn_blocking(|| -> std::io::Result<Vec<u8>> {
                let mut bytes = Vec::new();
                std::io::stdin()
                    .lock()
                    .take((MAX_SNAPSHOT_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)?;
                Ok(bytes)
            })
            .await??;
            let mut snapshot = parse_submission(&input)?;
            if title.is_some() {
                snapshot.title = title;
            }
            if source_url.is_some() {
                snapshot.source_url = source_url;
            }
            if stale_after.is_some() {
                snapshot.stale_after = stale_after;
            }
            snapshot.validate()?;
            let body = serde_json::to_vec(&snapshot)?;
            if body.len() > MAX_SNAPSHOT_BYTES {
                return Err("snapshot exceeds 4 MiB after applying options".into());
            }
            (
                "PUT",
                format!("/feeds/{name}"),
                body,
                Some((exit_added, exit_changed)),
            )
        }
        Command::Boards => ("GET", "/boards".into(), vec![], None),
        Command::Archive => ("GET", "/archive".into(), vec![], None),
        Command::Board { command } => {
            let req = board_cli::board(command, &paths, auto).await?;
            (req.method, req.resource, req.body, None)
        }
        Command::Todo { command } => {
            let req = board_cli::todo(command, &paths, auto).await?;
            (req.method, req.resource, req.body, None)
        }
        Command::Note { command } => {
            let req = board_cli::note(command, &paths, auto).await?;
            (req.method, req.resource, req.body, None)
        }
        Command::Feeds => ("GET", "/feeds".into(), vec![], None),
        Command::Get { name } => {
            validate_feed_name(&name)?;
            ("GET", format!("/feeds/{name}"), vec![], None)
        }
        Command::Fail { name, message } => {
            validate_feed_name(&name)?;
            let body = serde_json::to_vec(&serde_json::json!({"message": message}))?;
            if body.len() > 16 * 1024 {
                return Err("failure report exceeds 16 KiB".into());
            }
            ("POST", format!("/feeds/{name}/error"), body, None)
        }
        Command::Feed { command } => {
            let req = view_cli::feed(command, &paths, auto).await?;
            (req.method, req.resource, req.body, None)
        }
        Command::Layouts => ("GET", "/layouts".into(), vec![], None),
        Command::Layout { command } => {
            let req = view_cli::layout(command).await?;
            (req.method, req.resource, req.body, None)
        }
    };
    let (status, body) = client::request(&paths, method, &resource, body, auto).await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", String::from_utf8_lossy(&body)).into());
    }
    let code = if let Some((added, changed)) = exits {
        let summary: ChangeSummary = serde_json::from_slice(&body)?;
        if let Some(code) = added.filter(|_| summary.added > 0) {
            code
        } else if summary.added + summary.removed + summary.updated > 0 {
            changed.unwrap_or(0)
        } else {
            0
        }
    } else {
        0
    };
    let mut out = std::io::stdout().lock();
    out.write_all(&body)?;
    out.write_all(b"\n")?;
    Ok(code)
}
