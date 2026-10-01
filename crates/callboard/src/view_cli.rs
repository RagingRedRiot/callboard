//! CLI access to feed view state, promotion, and saved layouts.
use crate::board_cli::{Request, board_id};
use callboard::{Error, lifecycle::Paths};
use callboard_core::{
    feed::validate_feed_name,
    layout::{Layout, MAX_LAYOUT_BYTES, validate_name},
    store::FeedItemPatch,
};
use clap::{Subcommand, ValueEnum};
use std::{io::Read, path::Path};

#[derive(Subcommand)]
pub enum FeedCommand {
    Rm {
        name: String,
    },
    /// Apply item snooze/order fields from a JSON object on stdin.
    Patch {
        name: String,
        key: String,
    },
    /// Copy an item into a board, retaining its source reference.
    Promote {
        name: String,
        key: String,
        board: String,
        #[arg(long, value_enum, default_value = "todo")]
        kind: Kind,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Kind {
    Todo,
    Note,
}

#[derive(Subcommand)]
pub enum LayoutCommand {
    /// Create or replace a named layout with a {"view": ..., "cards": [...]} object from stdin.
    Save {
        name: String,
    },
    /// Rename a layout; an existing layout with the new name is never replaced.
    Rename {
        name: String,
        new_name: String,
    },
    Rm {
        name: String,
    },
}

// Encode UTF-8 bytes exactly once. Callers supply literal names and keys.
fn segment(value: &str) -> String {
    let mut result = String::new();
    const HEX: &[u8] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(byte as char);
        } else {
            result.push('%');
            result.push(HEX[(byte >> 4) as usize] as char);
            result.push(HEX[(byte & 15) as usize] as char);
        }
    }
    result
}

async fn input(limit: usize) -> Result<Vec<u8>, Error> {
    let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        std::io::stdin()
            .lock()
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    })
    .await??;
    if bytes.len() > limit {
        return Err(format!("input exceeds {limit} bytes").into());
    }
    Ok(bytes)
}

pub async fn feed(
    command: FeedCommand,
    paths: &Paths,
    auto: Option<&Path>,
) -> Result<Request, Error> {
    match command {
        FeedCommand::Rm { name } => {
            validate_feed_name(&name)?;
            Ok(Request {
                method: "DELETE",
                resource: format!("/feeds/{name}"),
                body: vec![],
            })
        }
        FeedCommand::Patch { name, key } => {
            validate_feed_name(&name)?;
            let body = input(16 * 1024).await?;
            let patch: FeedItemPatch = serde_json::from_slice(&body)?;
            if patch.position.is_some_and(|v| v < 0)
                || (patch.reset_order && patch.position.is_some())
            {
                return Err(
                    "position must be nonnegative and cannot be combined with reset_order".into(),
                );
            }
            Ok(Request {
                method: "PATCH",
                resource: format!("/feeds/{name}/items/{}", segment(&key)),
                body,
            })
        }
        FeedCommand::Promote {
            name,
            key,
            board,
            kind,
        } => {
            validate_feed_name(&name)?;
            let board_id = board_id(&board, paths, auto).await?;
            let kind = match kind {
                Kind::Todo => "todo",
                Kind::Note => "note",
            };
            Ok(Request {
                method: "POST",
                resource: format!("/feeds/{name}/items/{}/promote", segment(&key)),
                body: serde_json::to_vec(&serde_json::json!({"board_id":board_id,"kind":kind}))?,
            })
        }
    }
}

pub async fn layout(command: LayoutCommand) -> Result<Request, Error> {
    match command {
        LayoutCommand::Save { name } => {
            validate_name(&name)?;
            let body = input(MAX_LAYOUT_BYTES).await?;
            let layout: Layout = serde_json::from_slice(&body)?;
            layout.validate()?;
            Ok(Request {
                method: "PUT",
                resource: format!("/layouts/{}", segment(&name)),
                body,
            })
        }
        LayoutCommand::Rename { name, new_name } => {
            validate_name(&name)?;
            validate_name(&new_name)?;
            Ok(Request {
                method: "PATCH",
                resource: format!("/layouts/{}", segment(&name)),
                body: serde_json::to_vec(&serde_json::json!({"name":new_name}))?,
            })
        }
        LayoutCommand::Rm { name } => {
            validate_name(&name)?;
            Ok(Request {
                method: "DELETE",
                resource: format!("/layouts/{}", segment(&name)),
                body: vec![],
            })
        }
    }
}
