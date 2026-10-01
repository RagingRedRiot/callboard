//! Board and item command planning. All mutations go through the service API.
use callboard::{Error, client, lifecycle::Paths};
use callboard_core::{
    feed::{MAX_BODY_BYTES, MAX_TITLE_CHARS},
    store::{BoardInfo, NotePatch, TodoPatch},
};
use clap::Subcommand;
use serde_json::{Value, json};
use std::{io::Read, path::Path};

#[derive(Subcommand)]
pub enum BoardCommand {
    /// Create a named board.
    Add {
        name: String,
    },
    /// Read active board contents. BOARD is an ID or exact name.
    Get {
        board: String,
    },
    Rename {
        board: String,
        name: String,
    },
    /// Set a board's color (red, orange, yellow, green, blue, purple, pink,
    /// gray), or clear it with `none`.
    Color {
        board: String,
        color: String,
    },
    /// Delete a board; nonempty boards require --archive-contents.
    Rm {
        board: String,
        #[arg(long)]
        archive_contents: bool,
    },
    /// Read this board's archived items.
    Archive {
        board: String,
    },
}

#[derive(Subcommand)]
pub enum TodoCommand {
    Add {
        board: String,
        title: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        color: Option<String>,
    },
    /// Complete a todo, or undo completion with --undone.
    Done {
        id: i64,
        #[arg(long)]
        undone: bool,
    },
    #[command(flatten)]
    Item(ItemCommand),
}

#[derive(Subcommand)]
pub enum NoteCommand {
    Add {
        board: String,
        body: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        color: Option<String>,
    },
    #[command(flatten)]
    Item(ItemCommand),
}

#[derive(Subcommand)]
pub enum ItemCommand {
    /// Apply a JSON PATCH object from stdin, including null to clear fields.
    Patch {
        id: i64,
    },
    Archive {
        id: i64,
    },
    Restore {
        id: i64,
        board: String,
    },
    Move {
        id: i64,
        board: String,
    },
    Rm {
        id: i64,
    },
}

pub struct Request {
    pub method: &'static str,
    pub resource: String,
    pub body: Vec<u8>,
}
fn request(method: &'static str, resource: String, body: Value) -> Result<Request, Error> {
    let body = serde_json::to_vec(&body)?;
    if body.len() > 64 * 1024 {
        return Err("request exceeds 64 KiB".into());
    }
    Ok(Request {
        method,
        resource,
        body,
    })
}
fn valid_id(id: i64) -> Result<(), Error> {
    if id <= 0 {
        return Err("item ID must be positive".into());
    }
    Ok(())
}
fn content(title: Option<&str>, body: Option<&str>) -> Result<(), Error> {
    if title.is_some_and(|v| v.chars().count() > MAX_TITLE_CHARS) {
        return Err("title exceeds 500 characters".into());
    }
    if body.is_some_and(|v| v.len() > MAX_BODY_BYTES) {
        return Err("body exceeds 16 KiB".into());
    }
    Ok(())
}
pub(super) async fn board_id(
    board: &str,
    paths: &Paths,
    auto: Option<&Path>,
) -> Result<i64, Error> {
    if let Ok(id) = board.parse::<i64>() {
        if id <= 1 {
            return Err("board ID must be greater than 1".into());
        }
        return Ok(id);
    }
    if board.trim().is_empty() {
        return Err("board must not be empty".into());
    }
    let (status, bytes) = client::request(paths, "GET", "/boards", vec![], auto).await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", String::from_utf8_lossy(&bytes)).into());
    }
    let boards: Vec<BoardInfo> = serde_json::from_slice(&bytes)?;
    boards
        .into_iter()
        .find(|v| v.name == board)
        .map(|v| v.id)
        .ok_or_else(|| format!("board not found: {board}").into())
}

pub async fn board(
    command: BoardCommand,
    paths: &Paths,
    auto: Option<&Path>,
) -> Result<Request, Error> {
    match command {
        BoardCommand::Add { name } => {
            if name.trim().is_empty() {
                return Err("board name must not be empty".into());
            }
            request("POST", "/boards".into(), json!({"name":name}))
        }
        BoardCommand::Rename { board, name } => {
            if name.trim().is_empty() {
                return Err("board name must not be empty".into());
            }
            let mut req = request("PATCH", String::new(), json!({"name":name}))?;
            req.resource = format!("/boards/{}", board_id(&board, paths, auto).await?);
            Ok(req)
        }
        BoardCommand::Color { board, color } => {
            let color = (color != "none").then_some(color);
            let mut req = request("PATCH", String::new(), json!({ "color": color }))?;
            req.resource = format!("/boards/{}", board_id(&board, paths, auto).await?);
            Ok(req)
        }
        BoardCommand::Get { board } => request(
            "GET",
            format!("/boards/{}", board_id(&board, paths, auto).await?),
            Value::Null,
        ),
        BoardCommand::Archive { board } => request(
            "GET",
            format!("/boards/{}/archive", board_id(&board, paths, auto).await?),
            Value::Null,
        ),
        BoardCommand::Rm {
            board,
            archive_contents,
        } => request(
            "DELETE",
            format!("/boards/{}", board_id(&board, paths, auto).await?),
            json!({"archive_contents":archive_contents}),
        ),
    }
}

pub async fn todo(
    command: TodoCommand,
    paths: &Paths,
    auto: Option<&Path>,
) -> Result<Request, Error> {
    match command {
        TodoCommand::Add {
            board,
            title,
            body,
            url,
            color,
        } => {
            content(Some(&title), body.as_deref())?;
            let mut req = request(
                "POST",
                String::new(),
                json!({"title":title,"body":body,"url":url,"color":color}),
            )?;
            req.resource = format!("/boards/{}/todos", board_id(&board, paths, auto).await?);
            Ok(req)
        }
        TodoCommand::Done { id, undone } => {
            valid_id(id)?;
            request("PATCH", format!("/todos/{id}"), json!({"done":!undone}))
        }
        TodoCommand::Item(command) => item(command, "todos", paths, auto).await,
    }
}

pub async fn note(
    command: NoteCommand,
    paths: &Paths,
    auto: Option<&Path>,
) -> Result<Request, Error> {
    match command {
        NoteCommand::Add {
            board,
            title,
            body,
            url,
            color,
        } => {
            content(title.as_deref(), Some(&body))?;
            let mut req = request(
                "POST",
                String::new(),
                json!({"title":title,"body":body,"url":url,"color":color}),
            )?;
            req.resource = format!("/boards/{}/notes", board_id(&board, paths, auto).await?);
            Ok(req)
        }
        NoteCommand::Item(command) => item(command, "notes", paths, auto).await,
    }
}

async fn item(
    command: ItemCommand,
    kind: &str,
    paths: &Paths,
    auto: Option<&Path>,
) -> Result<Request, Error> {
    let id = match &command {
        ItemCommand::Patch { id }
        | ItemCommand::Archive { id }
        | ItemCommand::Restore { id, .. }
        | ItemCommand::Move { id, .. }
        | ItemCommand::Rm { id } => *id,
    };
    valid_id(id)?;
    let resource = format!("/{kind}/{id}");
    match command {
        ItemCommand::Patch { .. } => {
            let bytes = tokio::task::spawn_blocking(|| -> std::io::Result<Vec<u8>> {
                let mut bytes = Vec::new();
                std::io::stdin()
                    .lock()
                    .take(65537)
                    .read_to_end(&mut bytes)?;
                Ok(bytes)
            })
            .await??;
            if bytes.len() > 65536 {
                return Err("patch exceeds 64 KiB".into());
            }
            // Validate the typed contract, then retain explicit nulls and omitted fields.
            let (position, board_id) = if kind == "todos" {
                let patch: TodoPatch = serde_json::from_slice(&bytes)?;
                content(
                    patch.title.as_deref(),
                    patch.body.0.as_ref().and_then(Option::as_deref),
                )?;
                (patch.position, patch.board_id)
            } else {
                let patch: NotePatch = serde_json::from_slice(&bytes)?;
                content(
                    patch.title.0.as_ref().and_then(Option::as_deref),
                    patch.body.as_deref(),
                )?;
                (patch.position, patch.board_id)
            };
            if position.is_some_and(|v| v < 0) || board_id.is_some_and(|v| v <= 1) {
                return Err("invalid position or destination board ID".into());
            }
            Ok(Request {
                method: "PATCH",
                resource,
                body: bytes,
            })
        }
        ItemCommand::Archive { .. } => request("POST", format!("{resource}/archive"), json!({})),
        ItemCommand::Rm { .. } => request("DELETE", resource, Value::Null),
        ItemCommand::Restore { board, .. } => request(
            "POST",
            format!("{resource}/restore"),
            json!({"board_id":board_id(&board,paths,auto).await?}),
        ),
        ItemCommand::Move { board, .. } => request(
            "POST",
            format!("{resource}/move"),
            json!({"board_id":board_id(&board,paths,auto).await?}),
        ),
    }
}
