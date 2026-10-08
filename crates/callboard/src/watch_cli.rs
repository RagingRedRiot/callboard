//! CLI access to watches (DESIGN.md §4a). All requests are validated before
//! the service is contacted.
use crate::board_cli::Request;
use callboard_core::{
    feed::MAX_SNAPSHOT_BYTES,
    watch::{Failure, ItemAction, ItemReport, NewItem, Report, validate_watch_name},
};
use callboard_service::Error;
use clap::Subcommand;
use serde::Serialize;
use std::io::Read;

#[derive(Subcommand)]
pub enum WatchCommand {
    /// Report on items from a JSON object or array on stdin (empty input
    /// reports metadata only). The first report creates the watch.
    Report {
        name: String,
        #[arg(long)]
        title: Option<String>,
        /// What the watch follows, shown with it (at most 1000 characters).
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        source_url: Option<String>,
        #[arg(long)]
        stale_after: Option<String>,
        /// How long added items stay new, e.g. 1d, or never (default: 1d).
        #[arg(long)]
        waiting_after: Option<String>,
        /// How long items wait before they are quiet, e.g. 7d, or never
        /// (default: 7d).
        #[arg(long)]
        quiet_after: Option<String>,
        /// Exit with this code if items newly need attention (takes precedence).
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..))]
        exit_attention: Option<u8>,
        /// Exit with this code if items became quiet since the previous report.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..))]
        exit_quiet: Option<u8>,
    },
    /// Report a failed run for the watch, or for one item: fail NAME [ID] MESSAGE.
    Fail {
        name: String,
        #[arg(num_args = 1..=2, value_names = ["ID", "MESSAGE"], required = true)]
        args: Vec<String>,
    },
    /// Read a watch and its items, in queue order, as JSON.
    Items { name: String },
    /// Delete a watch and its items.
    Rm { name: String },
    Item {
        #[command(subcommand)]
        command: ItemCommand,
    },
    /// Acknowledge an item that needs attention.
    Ack { name: String, id: i64 },
    /// Keep a quiet item waiting, restarting its waiting clock.
    KeepWaiting { name: String, id: i64 },
}

#[derive(Subcommand)]
pub enum ItemCommand {
    /// Add a URL to watch; a URL already on the watch is left as it is.
    Add {
        name: String,
        url: String,
        /// Why you are watching it.
        #[arg(long)]
        label: Option<String>,
    },
    Rm {
        name: String,
        id: i64,
    },
}

/// Exit codes a report asks for: (attention, quiet).
pub type Exits = (Option<u8>, Option<u8>);

fn json(method: &'static str, resource: String, body: &impl Serialize) -> Result<Request, Error> {
    Ok(Request {
        method,
        resource,
        body: serde_json::to_vec(body)?,
    })
}

fn item_id(id: i64) -> Result<i64, Error> {
    if id <= 0 {
        return Err("item IDs are positive integers".into());
    }
    Ok(id)
}

/// Parse a report from stdin: an object, an array of item reports, or nothing.
fn parse_report(input: &[u8]) -> Result<Report, Error> {
    if input.len() > MAX_SNAPSHOT_BYTES {
        return Err("report exceeds 4 MiB".into());
    }
    Ok(match input.iter().find(|b| !b.is_ascii_whitespace()) {
        None => Report::default(),
        Some(b'[') => Report {
            items: serde_json::from_slice::<Vec<ItemReport>>(input)
                .map_err(|e| format!("invalid item reports JSON: {e}"))?,
            ..Default::default()
        },
        _ => serde_json::from_slice(input).map_err(|e| format!("invalid report JSON: {e}"))?,
    })
}

pub async fn watch(command: WatchCommand) -> Result<(Request, Option<Exits>), Error> {
    let name = match &command {
        WatchCommand::Report { name, .. }
        | WatchCommand::Fail { name, .. }
        | WatchCommand::Items { name }
        | WatchCommand::Rm { name }
        | WatchCommand::Ack { name, .. }
        | WatchCommand::KeepWaiting { name, .. }
        | WatchCommand::Item {
            command: ItemCommand::Add { name, .. } | ItemCommand::Rm { name, .. },
        } => name.clone(),
    };
    validate_watch_name(&name)?;
    let base = format!("/watches/{name}");
    let request = match command {
        WatchCommand::Report {
            title,
            description,
            source_url,
            stale_after,
            waiting_after,
            quiet_after,
            exit_attention,
            exit_quiet,
            ..
        } => {
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
            let mut report = parse_report(&input)?;
            for (option, field) in [
                (title, &mut report.title),
                (description, &mut report.description),
                (source_url, &mut report.source_url),
                (stale_after, &mut report.stale_after),
                (waiting_after, &mut report.waiting_after),
                (quiet_after, &mut report.quiet_after),
            ] {
                if option.is_some() {
                    *field = option;
                }
            }
            report.validate()?;
            let request = json("POST", format!("{base}/report"), &report)?;
            if request.body.len() > MAX_SNAPSHOT_BYTES {
                return Err("report exceeds 4 MiB after applying options".into());
            }
            return Ok((request, Some((exit_attention, exit_quiet))));
        }
        WatchCommand::Fail { mut args, .. } => {
            let message = args.pop().expect("clap requires a message");
            let id = args
                .pop()
                .map(|id| id.parse::<i64>().map_err(|_| "ID must be an integer"))
                .transpose()?
                .map(item_id)
                .transpose()?;
            json("POST", format!("{base}/error"), &Failure { id, message })?
        }
        WatchCommand::Items { .. } => Request {
            method: "GET",
            resource: base,
            body: vec![],
        },
        WatchCommand::Rm { .. } => Request {
            method: "DELETE",
            resource: base,
            body: vec![],
        },
        WatchCommand::Ack { id, .. } => {
            let action = ItemAction {
                acknowledge: true,
                keep_waiting: false,
            };
            json("PATCH", format!("{base}/items/{}", item_id(id)?), &action)?
        }
        WatchCommand::KeepWaiting { id, .. } => {
            let action = ItemAction {
                acknowledge: false,
                keep_waiting: true,
            };
            json("PATCH", format!("{base}/items/{}", item_id(id)?), &action)?
        }
        WatchCommand::Item {
            command: ItemCommand::Add { url, label, .. },
        } => {
            let item = NewItem { url, label };
            item.normalized()?;
            json("POST", format!("{base}/items"), &item)?
        }
        WatchCommand::Item {
            command: ItemCommand::Rm { id, .. },
        } => Request {
            method: "DELETE",
            resource: format!("{base}/items/{}", item_id(id)?),
            body: vec![],
        },
    };
    Ok((request, None))
}
