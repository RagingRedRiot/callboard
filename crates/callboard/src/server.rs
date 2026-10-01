//! Bounded HTTP/1 feed API over an authenticated Unix listener.
use crate::api::{Reply, error, reply};
use crate::{
    Error,
    lifecycle::{Paths, ServiceGuard},
};
use callboard_core::store::Store;
use http_body_util::BodyExt;
use hyper::{Method, Request, Response, StatusCode, body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::{
    convert::Infallible,
    future::Future,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::{UnixListener, UnixStream},
    task::JoinSet,
};

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

/// How the service was started (DESIGN.md §7.4).
#[derive(Debug, Default)]
pub struct Start {
    /// The installed binary an upgrade re-executes. `None` (an in-process
    /// service) refuses upgrades.
    pub executable: Option<PathBuf>,
    /// `LOCK,LISTENER` descriptors inherited from an upgrading predecessor.
    pub handoff: Option<String>,
}

/// One accepted upgrade, from its request handler to the accept loop.
struct Order {
    executable: PathBuf,
    reply: tokio::sync::oneshot::Sender<Reply>,
}

struct Shared {
    store: Store,
    event_slots: Arc<tokio::sync::Semaphore>,
    executable: Option<PathBuf>,
    orders: tokio::sync::mpsc::Sender<Order>,
    /// One upgrade at a time.
    upgrading: AtomicBool,
    /// Flips to true when the service stops accepting; event streams end.
    closing: tokio::sync::watch::Receiver<bool>,
}

pub async fn serve(
    paths: Paths,
    start: Start,
    shutdown: impl Future<Output = ()>,
) -> Result<(), Error> {
    let guard = match &start.handoff {
        Some(fds) => ServiceGuard::adopt(paths, fds)?,
        None => ServiceGuard::bind(paths)?,
    };
    // A route overlapping another fails startup, not a request.
    crate::api::table()?;
    let store = Store::open(guard.paths().database()).await?;
    let listener = UnixListener::from_std(guard.listener().try_clone()?)?;
    let mut connections = JoinSet::new();
    let (orders, mut order_rx) = tokio::sync::mpsc::channel(1);
    let (close, closing) = tokio::sync::watch::channel(false);
    let shared = Arc::new(Shared {
        store: store.clone(),
        event_slots: Arc::new(tokio::sync::Semaphore::new(crate::events::MAX_SUBSCRIBERS)),
        executable: start.executable,
        orders,
        upgrading: AtomicBool::new(false),
        closing,
    });
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break Ok(None),
            Some(order) = order_rx.recv() => break Ok(Some(order)),
            Some(_) = connections.join_next(), if !connections.is_empty() => (),
            accepted = listener.accept(), if connections.len() < 64 => {
                let (stream, _) = match accepted { Ok(v) => v, Err(e) => break Err(e) };
                if authenticate(&stream, rustix::process::geteuid().as_raw()).is_err() { continue; }
                let shared = shared.clone();
                connections.spawn(async move {
                    let service = service_fn(move |request| {
                        let shared = shared.clone();
                        async move { Ok::<_, Infallible>(handle(request, &shared).await) }
                    });
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.keep_alive(false).max_headers(32).max_buf_size(16 * 1024).header_read_timeout(None);
                    // Covers headers, body, storage work, and response writes.
                    let _ = tokio::time::timeout(Duration::from_secs(30), builder.serve_connection(TokioIo::new(stream), service)).await;
                });
            }
        }
    };
    // Stop accepting. The guard's listener stays open, so during an upgrade
    // new clients wait in the backlog for the next image.
    drop(listener);
    let _ = close.send(true);
    // Answer the upgrade before draining: its own connection is one of those
    // the drain waits for.
    let result = result.map(|order| {
        order.map(|order| {
            eprintln!("callboard: upgrading to {}", order.executable.display());
            let _ = order.reply.send(reply(
                StatusCode::ACCEPTED,
                json!({"upgrade":"started","executable":order.executable}),
            ));
            order.executable
        })
    });
    let drain = async {
        if tokio::time::timeout(Duration::from_secs(5), async {
            while connections.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        }
    };
    let mut upgrade = None;
    match &result {
        // A stop requested mid-drain wins: exit instead of re-executing.
        Ok(Some(executable)) => tokio::select! {
            biased;
            _ = &mut shutdown => eprintln!("callboard: stopped during upgrade; exiting"),
            () = drain => upgrade = Some(executable.clone()),
        },
        _ => drain.await,
    }
    store.close().await;
    if let Some(executable) = upgrade {
        let error = guard.exec(&executable);
        eprintln!(
            "callboard: could not execute {} ({error}); restarting the running build",
            executable.display()
        );
        let error = guard.exec(Path::new("/proc/self/exe"));
        return Err(format!("upgrade failed and could not restart: {error}").into());
    }
    drop(guard);
    result.map(|_| ()).map_err(Into::into)
}

/// `POST /service/upgrade` (DESIGN.md §7.4). The replies' shapes are frozen.
async fn request_upgrade(shared: &Shared) -> Reply {
    let abandoned = |reason: String| {
        reply(
            StatusCode::CONFLICT,
            json!({"upgrade":"abandoned","reason":reason}),
        )
    };
    let Some(executable) = &shared.executable else {
        return abandoned("this service runs in-process and has no binary to re-execute".into());
    };
    if shared.upgrading.swap(true, Ordering::SeqCst) {
        return abandoned("an upgrade is already in progress".into());
    }
    let outcome = match crate::upgrade::preflight(executable).await {
        Err(crate::upgrade::Preflight::Current) => {
            shared.upgrading.store(false, Ordering::SeqCst);
            return reply(
                StatusCode::OK,
                json!({"upgrade":"current","executable":executable}),
            );
        }
        Err(crate::upgrade::Preflight::Unusable(reason)) => {
            shared.upgrading.store(false, Ordering::SeqCst);
            return abandoned(reason);
        }
        Ok(()) => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let order = Order {
                executable: executable.clone(),
                reply: tx,
            };
            if shared.orders.send(order).await.is_err() {
                return abandoned("the service is shutting down".into());
            }
            rx.await
        }
    };
    outcome.unwrap_or_else(|_| abandoned("the service stopped before the upgrade began".into()))
}
async fn handle(request: Request<Incoming>, shared: &Shared) -> Reply {
    let method = request.method().clone();
    match request.uri().path() {
        "/service/upgrade" if method == Method::POST => request_upgrade(shared).await,
        "/events" if method == Method::GET => match crate::events::EventBody::new(
            &shared.store,
            shared.event_slots.clone(),
            shared.closing.clone(),
        ) {
            Some(body) => Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("x-accel-buffering", "no")
                .header("x-callboard-build", crate::BUILD)
                .body(body.boxed_unsync())
                .unwrap(),
            None => error(
                StatusCode::SERVICE_UNAVAILABLE,
                "event subscriber limit reached; retry later",
            ),
        },
        "/health" if method == Method::GET => reply(
            StatusCode::OK,
            json!({"service":"callboard", "api":1, "version":crate::VERSION, "build":crate::BUILD}),
        ),
        "/service/upgrade" | "/events" | "/health" => {
            error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
        }
        // Feature routes live in src/routes/ (DESIGN.md §8.1).
        _ => crate::api::dispatch(request, &shared.store).await,
    }
}
