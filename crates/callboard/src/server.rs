//! Bounded HTTP/1 feed API over an authenticated Unix listener.
use crate::{
    Error,
    lifecycle::{Paths, ServiceGuard},
};
use callboard_core::{
    feed::{MAX_SNAPSHOT_BYTES, Snapshot, validate_feed_name},
    store::{
        FeedItemPatch, NotePatch, PreferencesPatch, SourceReference, Store, StoreError, TodoPatch,
    },
};
use http_body_util::{BodyExt, Full, Limited, combinators::UnsyncBoxBody};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::json;
use std::{convert::Infallible, future::Future, sync::Arc, time::Duration};
use tokio::{
    net::{UnixListener, UnixStream},
    task::JoinSet,
};

type Reply = Response<UnsyncBoxBody<Bytes, Infallible>>;

struct RequestError {
    status: StatusCode,
    message: String,
}

impl RequestError {
    fn reply(self) -> Reply {
        error(self.status, self.message)
    }
}

/// Both clients and the service authenticate the kernel-reported peer UID.
pub fn authenticate(stream: &UnixStream, expected_uid: u32) -> std::io::Result<()> {
    if stream.peer_cred()?.uid() == expected_uid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "peer UID does not match service user",
        ))
    }
}

pub async fn serve(paths: Paths, shutdown: impl Future<Output = ()>) -> Result<(), Error> {
    let guard = ServiceGuard::bind(paths)?;
    let store = Store::open(guard.paths().database()).await?;
    let listener = UnixListener::from_std(guard.listener().try_clone()?)?;
    let mut connections = JoinSet::new();
    let event_slots = Arc::new(tokio::sync::Semaphore::new(crate::events::MAX_SUBSCRIBERS));
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break Ok(()),
            Some(_) = connections.join_next(), if !connections.is_empty() => (),
            accepted = listener.accept(), if connections.len() < 64 => {
                let (stream, _) = match accepted { Ok(v) => v, Err(e) => break Err(e) };
                if authenticate(&stream, rustix::process::geteuid().as_raw()).is_err() { continue; }
                let store = store.clone();
                let event_slots = event_slots.clone();
                connections.spawn(async move {
                    let service = service_fn(move |request| {
                        let store = store.clone();
                        let event_slots = event_slots.clone();
                        async move { Ok::<_, Infallible>(handle(request, &store, event_slots).await) }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.keep_alive(false).max_headers(32).max_buf_size(16 * 1024).header_read_timeout(None);
                    // Covers headers, body, storage work, and response writes.
                    let _ = tokio::time::timeout(Duration::from_secs(30), builder.serve_connection(TokioIo::new(stream), service)).await;
                });
            }
        }
    };
    drop(listener);
    if tokio::time::timeout(Duration::from_secs(5), async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    store.close().await;
    drop(guard);
    result.map_err(Into::into)
}

fn reply(status: StatusCode, value: impl serde::Serialize) -> Reply {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(
            Full::new(Bytes::from(
                serde_json::to_vec(&value).expect("JSON response serialization"),
            ))
            .boxed_unsync(),
        )
        .unwrap()
}
fn error(status: StatusCode, message: impl ToString) -> Reply {
    reply(status, json!({"error":message.to_string()}))
}
fn storage_error(e: StoreError) -> Reply {
    match e {
        StoreError::Validation(_) | StoreError::Layout(_) | StoreError::EmptyBoardName => {
            error(StatusCode::BAD_REQUEST, e)
        }
        StoreError::SnapshotTooLarge | StoreError::LayoutTooLarge => {
            error(StatusCode::PAYLOAD_TOO_LARGE, e)
        }
        StoreError::FeedNotFound(_)
        | StoreError::FeedItemNotFound { .. }
        | StoreError::BoardNotFound(_)
        | StoreError::BoardItemNotFound { .. }
        | StoreError::LayoutNotFound(_) => error(StatusCode::NOT_FOUND, e),
        StoreError::BoardNotEmpty(_)
        | StoreError::BoardNameTaken
        | StoreError::LayoutNameTaken(_) => error(StatusCode::CONFLICT, e),
        StoreError::InvalidPosition(_)
        | StoreError::RestoreBoardRequired
        | StoreError::InvalidPatch(_)
        | StoreError::ContentLimit(_) => error(StatusCode::BAD_REQUEST, e),
        _ => {
            eprintln!("callboard storage: {e}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "storage failure")
        }
    }
}

async fn handle(
    request: Request<Incoming>,
    store: &Store,
    event_slots: Arc<tokio::sync::Semaphore>,
) -> Reply {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let parts: Vec<_> = path.split('/').collect();
    if path == "/events" {
        if method != Method::GET {
            return error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
        }
        return match crate::events::EventBody::new(store, event_slots) {
            Some(body) => Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("x-accel-buffering", "no")
                .body(body.boxed_unsync())
                .unwrap(),
            None => error(
                StatusCode::SERVICE_UNAVAILABLE,
                "event subscriber limit reached; retry later",
            ),
        };
    }

    if matches!(
        parts.get(1),
        Some(&"boards" | &"todos" | &"notes" | &"archive")
    ) {
        return handle_board_api(request, store, &parts).await;
    }
    if parts.get(1) == Some(&"layouts") {
        return match parts.as_slice() {
            ["", "layouts"] if method == Method::GET => match store.list_layouts().await {
                Ok(layouts) => reply(StatusCode::OK, layouts),
                Err(e) => storage_error(e),
            },
            ["", "layouts", name] if method == Method::PUT => {
                let name = match decode_path_segment(name) {
                    Ok(name) => name,
                    Err(e) => return error(StatusCode::BAD_REQUEST, e),
                };
                let layout = match request_json::<callboard_core::layout::Layout>(request).await {
                    Ok(layout) => layout,
                    Err(e) => return e.reply(),
                };
                match store.save_layout(&name, &layout).await {
                    Ok(layout) => reply(StatusCode::OK, layout),
                    Err(e) => storage_error(e),
                }
            }
            ["", "layouts", name] if method == Method::DELETE => {
                let name = match decode_path_segment(name) {
                    Ok(name) => name,
                    Err(e) => return error(StatusCode::BAD_REQUEST, e),
                };
                match store.delete_layout(&name).await {
                    Ok(deleted) => reply(StatusCode::OK, json!({"deleted":deleted})),
                    Err(e) => storage_error(e),
                }
            }
            ["", "layouts", name] if method == Method::PATCH => {
                let name = match decode_path_segment(name) {
                    Ok(name) => name,
                    Err(e) => return error(StatusCode::BAD_REQUEST, e),
                };
                let rename =
                    match request_json::<callboard_core::layout::LayoutRename>(request).await {
                        Ok(rename) => rename,
                        Err(e) => return e.reply(),
                    };
                match store.rename_layout(&name, &rename.name).await {
                    Ok(layout) => reply(StatusCode::OK, layout),
                    Err(e) => storage_error(e),
                }
            }
            ["", "layouts"] | ["", "layouts", _] => {
                error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
            }
            _ => error(StatusCode::NOT_FOUND, "unknown resource"),
        };
    }
    if parts.as_slice() == ["", "preferences"] {
        return match method {
            Method::GET => match store.preferences().await {
                Ok(preferences) => reply(StatusCode::OK, preferences),
                Err(e) => storage_error(e),
            },
            Method::PATCH => {
                let patch = match request_json::<PreferencesPatch>(request).await {
                    Ok(patch) => patch,
                    Err(e) => return e.reply(),
                };
                match store.patch_preferences(patch).await {
                    Ok(preferences) => reply(StatusCode::OK, preferences),
                    Err(e) => storage_error(e),
                }
            }
            _ => error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
        };
    }
    let route = match parts.as_slice() {
        ["", "health"] => 0,
        ["", "feeds"] => 1,
        ["", "feeds", name] if validate_feed_name(name).is_ok() => 2,
        ["", "feeds", name, "error"] if validate_feed_name(name).is_ok() => 3,
        ["", "feeds", name, "items", _key] if validate_feed_name(name).is_ok() => 4,
        ["", "feeds", name, "items", _key, "promote"] if validate_feed_name(name).is_ok() => 5,
        _ => return error(StatusCode::NOT_FOUND, "unknown resource"),
    };
    let allowed = match route {
        0 | 1 => method == Method::GET,
        2 => matches!(method, Method::GET | Method::PUT | Method::DELETE),
        3 => method == Method::POST,
        4 => method == Method::PATCH,
        _ => method == Method::POST,
    };
    if !allowed {
        return error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    if route == 0 {
        return reply(StatusCode::OK, json!({"service":"callboard", "api":1}));
    }
    if route == 1 {
        return match store.feed_summaries().await {
            Ok(v) => reply(StatusCode::OK, v),
            Err(e) => storage_error(e),
        };
    }
    let name = parts[2];
    let item_key = if route == 4 || route == 5 {
        match decode_path_segment(parts[4]) {
            Ok(key) => key,
            Err(message) => return error(StatusCode::BAD_REQUEST, message),
        }
    } else {
        String::new()
    };
    if method == Method::GET {
        return match store.feed(name).await {
            Ok(Some(v)) => reply(StatusCode::OK, v),
            Ok(None) => error(StatusCode::NOT_FOUND, "feed not found"),
            Err(e) => storage_error(e),
        };
    }
    if method == Method::DELETE {
        return match store.delete_feed(name).await {
            Ok(v) => reply(StatusCode::OK, json!({"deleted":v})),
            Err(e) => storage_error(e),
        };
    }
    let limit = if (3..=5).contains(&route) {
        16 * 1024
    } else {
        MAX_SNAPSHOT_BYTES
    };
    if request
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > limit as u64)
    {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds limit");
    }
    let body = match tokio::time::timeout(
        Duration::from_secs(10),
        Limited::new(request.into_body(), limit).collect(),
    )
    .await
    {
        Err(_) => return error(StatusCode::REQUEST_TIMEOUT, "request body timed out"),
        Ok(Err(e)) if e.is::<http_body_util::LengthLimitError>() => {
            return error(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds limit");
        }
        Ok(Err(_)) => return error(StatusCode::BAD_REQUEST, "invalid request body"),
        Ok(Ok(v)) => v.to_bytes(),
    };
    if route == 3 {
        #[derive(Deserialize)]
        struct Failure {
            message: String,
        }
        let failure: Failure = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return error(StatusCode::BAD_REQUEST, e),
        };
        return match store.report_error(name, &failure.message).await {
            Ok(()) => reply(StatusCode::OK, json!({"recorded":true})),
            Err(e) => storage_error(e),
        };
    }
    if route == 4 {
        let patch: FeedItemPatch = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return error(StatusCode::BAD_REQUEST, e),
        };
        return match store.patch_feed_item(name, &item_key, patch).await {
            Ok(v) => reply(StatusCode::OK, v),
            Err(e) => storage_error(e),
        };
    }
    if route == 5 {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Kind {
            Todo,
            Note,
        }
        #[derive(Deserialize)]
        struct Promote {
            board_id: i64,
            kind: Kind,
        }
        let request: Promote = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return error(StatusCode::BAD_REQUEST, e),
        };
        let result = match request.kind {
            Kind::Todo => store
                .promote_todo(name, &item_key, request.board_id)
                .await
                .map(|v| json!(v)),
            Kind::Note => store
                .promote_note(name, &item_key, request.board_id)
                .await
                .map(|v| json!(v)),
        };
        return match result {
            Ok(v) => reply(StatusCode::CREATED, v),
            Err(e) => storage_error(e),
        };
    }
    let snapshot: Snapshot = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    match store.submit(name, &snapshot).await {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => storage_error(e),
    }
}

async fn request_json<T: for<'de> Deserialize<'de>>(
    request: Request<Incoming>,
) -> Result<T, RequestError> {
    let body = request_body(request).await?;
    serde_json::from_slice(&body).map_err(|e| RequestError {
        status: StatusCode::BAD_REQUEST,
        message: e.to_string(),
    })
}

async fn request_json_default<T: for<'de> Deserialize<'de> + Default>(
    request: Request<Incoming>,
) -> Result<T, RequestError> {
    let body = request_body(request).await?;
    if body.is_empty() {
        Ok(T::default())
    } else {
        serde_json::from_slice(&body).map_err(|e| RequestError {
            status: StatusCode::BAD_REQUEST,
            message: e.to_string(),
        })
    }
}

async fn request_body(request: Request<Incoming>) -> Result<Bytes, RequestError> {
    if request
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > 64 * 1024)
    {
        return Err(RequestError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "request body exceeds limit".into(),
        });
    }
    let body = match tokio::time::timeout(
        Duration::from_secs(10),
        Limited::new(request.into_body(), 64 * 1024).collect(),
    )
    .await
    {
        Err(_) => {
            return Err(RequestError {
                status: StatusCode::REQUEST_TIMEOUT,
                message: "request body timed out".into(),
            });
        }
        Ok(Err(e)) if e.is::<http_body_util::LengthLimitError>() => {
            return Err(RequestError {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                message: "request body exceeds limit".into(),
            });
        }
        Ok(Err(_)) => {
            return Err(RequestError {
                status: StatusCode::BAD_REQUEST,
                message: "invalid request body".into(),
            });
        }
        Ok(Ok(v)) => v.to_bytes(),
    };
    Ok(body)
}

async fn handle_board_api(request: Request<Incoming>, store: &Store, parts: &[&str]) -> Reply {
    let allowed: &[Method] = match parts {
        ["", "archive"] => &[Method::GET],
        ["", "boards"] => &[Method::GET, Method::POST],
        ["", "boards", id] if id.parse::<i64>().is_ok() => {
            &[Method::GET, Method::PATCH, Method::DELETE]
        }
        ["", "boards", id, "archive"] if id.parse::<i64>().is_ok() => &[Method::GET],
        ["", "boards", id, "todos" | "notes"] if id.parse::<i64>().is_ok() => &[Method::POST],
        ["", "todos" | "notes", id] if id.parse::<i64>().is_ok() => {
            &[Method::PATCH, Method::DELETE]
        }
        ["", "todos" | "notes", id, "archive" | "restore" | "move"]
            if id.parse::<i64>().is_ok() =>
        {
            &[Method::POST]
        }
        _ => return error(StatusCode::NOT_FOUND, "unknown resource"),
    };
    if !allowed.contains(request.method()) {
        return error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let method = request.method().clone();
    match parts {
        ["", "archive"] if method == Method::GET => match store.board_contents(None, true).await {
            Ok(contents) => reply(StatusCode::OK, contents),
            Err(e) => storage_error(e),
        },
        ["", "boards"] if method == Method::GET => match store.board_summaries().await {
            Ok(v) => reply(StatusCode::OK, v),
            Err(e) => storage_error(e),
        },
        ["", "boards"] if method == Method::POST => {
            #[derive(Deserialize)]
            struct Create {
                name: String,
            }
            let body = match request_json::<Create>(request).await {
                Ok(v) => v,
                Err(e) => return e.reply(),
            };
            match store.create_board(&body.name).await {
                Ok(v) => reply(StatusCode::CREATED, v),
                Err(e) => storage_error(e),
            }
        }
        ["", "boards", id] if id.parse::<i64>().is_ok() && method == Method::GET => {
            let id = id.parse().unwrap();
            match store.board_contents(Some(id), false).await {
                Ok(v) => reply(StatusCode::OK, v),
                Err(e) => storage_error(e),
            }
        }
        ["", "boards", id] if id.parse::<i64>().is_ok() && method == Method::PATCH => {
            #[derive(Deserialize)]
            struct Rename {
                name: String,
            }
            let id = id.parse().unwrap();
            let body = match request_json::<Rename>(request).await {
                Ok(v) => v,
                Err(e) => return e.reply(),
            };
            match store.rename_board(id, &body.name).await {
                Ok(()) => reply(StatusCode::OK, json!({"id":id,"name":body.name.trim()})),
                Err(e) => storage_error(e),
            }
        }
        ["", "boards", id] if id.parse::<i64>().is_ok() && method == Method::DELETE => {
            #[derive(Deserialize, Default)]
            struct Delete {
                #[serde(default)]
                archive_contents: bool,
            }
            let id = id.parse().unwrap();
            let body = match request_json_default::<Delete>(request).await {
                Ok(v) => v,
                Err(e) => return e.reply(),
            };
            match store.delete_board(id, body.archive_contents).await {
                Ok(()) => reply(StatusCode::OK, json!({"deleted": true})),
                Err(e) => storage_error(e),
            }
        }
        ["", "boards", id, "archive"] if id.parse::<i64>().is_ok() && method == Method::GET => {
            let id = id.parse().unwrap();
            match store.board_contents(Some(id), true).await {
                Ok(v) => reply(StatusCode::OK, v),
                Err(e) => storage_error(e),
            }
        }
        ["", "boards", id, "todos"] if id.parse::<i64>().is_ok() && method == Method::POST => {
            #[derive(Deserialize)]
            struct Create {
                title: String,
                body: Option<String>,
                url: Option<String>,
                reference: Option<SourceReference>,
            }
            let id = id.parse().unwrap();
            let body = match request_json::<Create>(request).await {
                Ok(v) => v,
                Err(e) => return e.reply(),
            };
            match store
                .add_todo(
                    id,
                    &body.title,
                    body.body.as_deref(),
                    body.url.as_deref(),
                    body.reference.as_ref(),
                )
                .await
            {
                Ok(v) => reply(StatusCode::CREATED, v),
                Err(e) => storage_error(e),
            }
        }
        ["", "boards", id, "notes"] if id.parse::<i64>().is_ok() && method == Method::POST => {
            #[derive(Deserialize)]
            struct Create {
                title: Option<String>,
                body: String,
                url: Option<String>,
                color: Option<String>,
                reference: Option<SourceReference>,
            }
            let id = id.parse().unwrap();
            let body = match request_json::<Create>(request).await {
                Ok(v) => v,
                Err(e) => return e.reply(),
            };
            match store
                .add_note_with_url(
                    id,
                    body.title.as_deref(),
                    &body.body,
                    body.url.as_deref(),
                    body.color.as_deref(),
                    body.reference.as_ref(),
                )
                .await
            {
                Ok(v) => reply(StatusCode::CREATED, v),
                Err(e) => storage_error(e),
            }
        }
        ["", "todos", id] if id.parse::<i64>().is_ok() && method == Method::PATCH => {
            let id = id.parse().unwrap();
            let body = match request_json::<TodoPatch>(request).await {
                Ok(v) => v,
                Err(e) => return e.reply(),
            };
            match store.patch_todo(id, body).await {
                Ok(v) => reply(StatusCode::OK, v),
                Err(e) => storage_error(e),
            }
        }
        ["", "notes", id] if id.parse::<i64>().is_ok() && method == Method::PATCH => {
            let id = id.parse().unwrap();
            let body = match request_json::<NotePatch>(request).await {
                Ok(v) => v,
                Err(e) => return e.reply(),
            };
            match store.patch_note(id, body).await {
                Ok(v) => reply(StatusCode::OK, v),
                Err(e) => storage_error(e),
            }
        }
        ["", kind @ ("todos" | "notes"), id, action] if id.parse::<i64>().is_ok() => {
            let id = id.parse().unwrap();
            match (method, *kind, *action) {
                (Method::POST, "todos", "archive") => unit(store.archive_todo(id).await),
                (Method::POST, "notes", "archive") => unit(store.archive_note(id).await),
                (Method::POST, "todos" | "notes", "restore") => {
                    #[derive(Deserialize)]
                    struct Restore {
                        board_id: i64,
                    }
                    let body = match request_json::<Restore>(request).await {
                        Ok(v) => v,
                        Err(e) => return e.reply(),
                    };
                    if *kind == "todos" {
                        item_reply(store.restore_todo(id, body.board_id).await)
                    } else {
                        item_reply(store.restore_note(id, body.board_id).await)
                    }
                }
                (Method::POST, "todos" | "notes", "move") => {
                    #[derive(Deserialize)]
                    struct Move {
                        board_id: i64,
                    }
                    let body = match request_json::<Move>(request).await {
                        Ok(v) => v,
                        Err(e) => return e.reply(),
                    };
                    if *kind == "todos" {
                        item_reply(store.move_todo(id, body.board_id).await)
                    } else {
                        item_reply(store.move_note(id, body.board_id).await)
                    }
                }
                _ => error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
            }
        }
        ["", "todos", id] if id.parse::<i64>().is_ok() && method == Method::DELETE => {
            unit(store.delete_todo(id.parse().unwrap()).await)
        }
        ["", "notes", id] if id.parse::<i64>().is_ok() && method == Method::DELETE => {
            unit(store.delete_note(id.parse().unwrap()).await)
        }
        _ => error(StatusCode::NOT_FOUND, "unknown resource"),
    }
}

fn unit(result: Result<(), StoreError>) -> Reply {
    match result {
        Ok(()) => reply(StatusCode::OK, json!({"ok":true})),
        Err(e) => storage_error(e),
    }
}

fn item_reply<T: serde::Serialize>(result: Result<T, StoreError>) -> Reply {
    match result {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => storage_error(e),
    }
}

/// Decode a single URI path segment, exactly once. '+' is literal in paths.
fn decode_path_segment(segment: &str) -> Result<String, &'static str> {
    let mut bytes = Vec::with_capacity(segment.len());
    let mut input = segment.bytes();
    while let Some(byte) = input.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let hi = input.next().and_then(|b| (b as char).to_digit(16));
        let lo = input.next().and_then(|b| (b as char).to_digit(16));
        match (hi, lo) {
            (Some(hi), Some(lo)) => bytes.push((hi * 16 + lo) as u8),
            _ => return Err("invalid percent escape in item key"),
        }
    }
    String::from_utf8(bytes).map_err(|_| "item key must be UTF-8")
}
