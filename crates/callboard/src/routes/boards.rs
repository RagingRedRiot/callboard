//! Boards, todos, notes, and the archive (DESIGN.md §5).
use crate::api::{BODY, Call, Reply, Route, done, route, stored};
use callboard_core::store::{BoardPatch, NotePatch, SourceReference, TodoPatch};
use hyper::StatusCode;
use serde::Deserialize;
use serde_json::json;

pub(crate) const ROUTES: &[Route] = &[
    route!(GET "/archive" => archive),
    route!(GET "/boards" => list),
    route!(POST "/boards" => create),
    route!(GET "/boards/{id}" => get),
    route!(PATCH "/boards/{id}" => patch),
    route!(DELETE "/boards/{id}" => delete),
    route!(GET "/boards/{id}/archive" => board_archive),
    route!(POST "/boards/{id}/todos" => add_todo),
    route!(POST "/boards/{id}/notes" => add_note),
    route!(PATCH "/todos/{id}" => patch_todo),
    route!(DELETE "/todos/{id}" => delete_todo),
    route!(POST "/todos/{id}/archive" => archive_todo),
    route!(POST "/todos/{id}/restore" => restore_todo),
    route!(POST "/todos/{id}/move" => move_todo),
    route!(PATCH "/notes/{id}" => patch_note),
    route!(DELETE "/notes/{id}" => delete_note),
    route!(POST "/notes/{id}/archive" => archive_note),
    route!(POST "/notes/{id}/restore" => restore_note),
    route!(POST "/notes/{id}/move" => move_note),
];

/// Read the JSON body or return its error reply.
macro_rules! body {
    ($call:expr) => {
        match $call.json(BODY).await {
            Ok(v) => v,
            Err(rejected) => return rejected.into(),
        }
    };
}

#[derive(Deserialize)]
struct Target {
    board_id: i64,
}

async fn archive(call: Call) -> Reply {
    stored(StatusCode::OK, call.store.board_contents(None, true).await)
}

async fn list(call: Call) -> Reply {
    stored(StatusCode::OK, call.store.board_summaries().await)
}

async fn create(mut call: Call) -> Reply {
    #[derive(Deserialize)]
    struct Create {
        name: String,
    }
    let body: Create = body!(call);
    stored(
        StatusCode::CREATED,
        call.store.create_board(&body.name).await,
    )
}

async fn get(call: Call) -> Reply {
    let contents = call.store.board_contents(Some(call.id()), false).await;
    stored(StatusCode::OK, contents)
}

async fn patch(mut call: Call) -> Reply {
    let patch: BoardPatch = body!(call);
    stored(
        StatusCode::OK,
        call.store.patch_board(call.id(), patch).await,
    )
}

async fn delete(mut call: Call) -> Reply {
    #[derive(Deserialize, Default)]
    struct Delete {
        #[serde(default)]
        archive_contents: bool,
    }
    let body: Delete = match call.json_or_default(BODY).await {
        Ok(v) => v,
        Err(rejected) => return rejected.into(),
    };
    let deleted = call
        .store
        .delete_board(call.id(), body.archive_contents)
        .await;
    stored(StatusCode::OK, deleted.map(|()| json!({"deleted": true})))
}

async fn board_archive(call: Call) -> Reply {
    let contents = call.store.board_contents(Some(call.id()), true).await;
    stored(StatusCode::OK, contents)
}

async fn add_todo(mut call: Call) -> Reply {
    #[derive(Deserialize)]
    struct Create {
        title: String,
        body: Option<String>,
        url: Option<String>,
        color: Option<String>,
        reference: Option<SourceReference>,
    }
    let body: Create = body!(call);
    let todo = call
        .store
        .add_todo_with(
            call.id(),
            &body.title,
            body.body.as_deref(),
            body.url.as_deref(),
            body.color.as_deref(),
            body.reference.as_ref(),
        )
        .await;
    stored(StatusCode::CREATED, todo)
}

async fn add_note(mut call: Call) -> Reply {
    #[derive(Deserialize)]
    struct Create {
        title: Option<String>,
        body: String,
        url: Option<String>,
        color: Option<String>,
        reference: Option<SourceReference>,
    }
    let body: Create = body!(call);
    let note = call
        .store
        .add_note_with_url(
            call.id(),
            body.title.as_deref(),
            &body.body,
            body.url.as_deref(),
            body.color.as_deref(),
            body.reference.as_ref(),
        )
        .await;
    stored(StatusCode::CREATED, note)
}

async fn patch_todo(mut call: Call) -> Reply {
    let patch: TodoPatch = body!(call);
    stored(
        StatusCode::OK,
        call.store.patch_todo(call.id(), patch).await,
    )
}

async fn delete_todo(call: Call) -> Reply {
    done(call.store.delete_todo(call.id()).await)
}

async fn archive_todo(call: Call) -> Reply {
    done(call.store.archive_todo(call.id()).await)
}

async fn restore_todo(mut call: Call) -> Reply {
    let target: Target = body!(call);
    let todo = call.store.restore_todo(call.id(), target.board_id).await;
    stored(StatusCode::OK, todo)
}

async fn move_todo(mut call: Call) -> Reply {
    let target: Target = body!(call);
    stored(
        StatusCode::OK,
        call.store.move_todo(call.id(), target.board_id).await,
    )
}

async fn patch_note(mut call: Call) -> Reply {
    let patch: NotePatch = body!(call);
    stored(
        StatusCode::OK,
        call.store.patch_note(call.id(), patch).await,
    )
}

async fn delete_note(call: Call) -> Reply {
    done(call.store.delete_note(call.id()).await)
}

async fn archive_note(call: Call) -> Reply {
    done(call.store.archive_note(call.id()).await)
}

async fn restore_note(mut call: Call) -> Reply {
    let target: Target = body!(call);
    let note = call.store.restore_note(call.id(), target.board_id).await;
    stored(StatusCode::OK, note)
}

async fn move_note(mut call: Call) -> Reply {
    let target: Target = body!(call);
    stored(
        StatusCode::OK,
        call.store.move_note(call.id(), target.board_id).await,
    )
}
