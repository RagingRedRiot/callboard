//! Bounded HTTP/1 feed API over an authenticated Unix listener.
use crate::{
    Error,
    lifecycle::{Paths, ServiceGuard},
};
use callboard_core::{
    feed::{MAX_SNAPSHOT_BYTES, Snapshot, validate_feed_name},
    store::{Store, StoreError},
};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::json;
use std::{convert::Infallible, future::Future, time::Duration};
use tokio::{
    net::{UnixListener, UnixStream},
    task::JoinSet,
};

type Reply = Response<Full<Bytes>>;

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
                connections.spawn(async move {
                    let service = service_fn(move |request| {
                        let store = store.clone();
                        async move { Ok::<_, Infallible>(handle(request, &store).await) }
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
        .body(Full::new(Bytes::from(
            serde_json::to_vec(&value).expect("JSON response serialization"),
        )))
        .unwrap()
}
fn error(status: StatusCode, message: impl ToString) -> Reply {
    reply(status, json!({"error":message.to_string()}))
}
fn storage_error(e: StoreError) -> Reply {
    match e {
        StoreError::Validation(_) => error(StatusCode::BAD_REQUEST, e),
        StoreError::SnapshotTooLarge => error(StatusCode::PAYLOAD_TOO_LARGE, e),
        StoreError::FeedNotFound(_) => error(StatusCode::NOT_FOUND, e),
        _ => {
            eprintln!("callboard storage: {e}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "storage failure")
        }
    }
}

async fn handle(request: Request<Incoming>, store: &Store) -> Reply {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let parts: Vec<_> = path.split('/').collect();
    let route = match parts.as_slice() {
        ["", "health"] => 0,
        ["", "feeds"] => 1,
        ["", "feeds", name] if validate_feed_name(name).is_ok() => 2,
        ["", "feeds", name, "error"] if validate_feed_name(name).is_ok() => 3,
        _ => return error(StatusCode::NOT_FOUND, "unknown resource"),
    };
    let allowed = match route {
        0 | 1 => method == Method::GET,
        2 => matches!(method, Method::GET | Method::PUT | Method::DELETE),
        _ => method == Method::POST,
    };
    if !allowed {
        return error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    if route == 0 {
        return reply(StatusCode::OK, json!({"service":"callboard", "api":1}));
    }
    if route == 1 {
        return match store.list_feeds().await {
            Ok(v) => reply(StatusCode::OK, v),
            Err(e) => storage_error(e),
        };
    }
    let name = parts[2];
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
    let limit = if route == 3 {
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
    let snapshot: Snapshot = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    match store.submit(name, &snapshot).await {
        Ok(v) => reply(StatusCode::OK, v),
        Err(e) => storage_error(e),
    }
}
