//! Bounded SSE bodies. The connection lifetime stays within the server timeout.
use callboard_core::store::{Change, Store};
use hyper::body::{Body, Bytes, Frame};
use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch};

pub(crate) const MAX_SUBSCRIBERS: usize = 16;
type Receive = Pin<Box<dyn Future<Output = (Option<Bytes>, broadcast::Receiver<Change>)> + Send>>;

fn receive(mut receiver: broadcast::Receiver<Change>) -> Receive {
    Box::pin(async move {
        let frame = match tokio::time::timeout(Duration::from_secs(10), receiver.recv()).await {
            Err(_) => Some(Bytes::from_static(b": heartbeat\n\n")),
            Ok(Ok(change)) => Some(Bytes::from(format!(
                "event: change\ndata: {}\n\n",
                serde_json::to_string(&change).expect("change serialization")
            ))),
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                // Discard the stale backlog. The subscriber refetches everything.
                receiver = receiver.resubscribe();
                Some(Bytes::from_static(b"event: resync\ndata: {}\n\n"))
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => None,
        };
        (frame, receiver)
    })
}

pub(crate) struct EventBody {
    pending: Receive,
    /// Resolves when the service stops accepting; the stream then ends.
    closing: Pin<Box<dyn Future<Output = ()> + Send>>,
    initial: bool,
    deadline: Pin<Box<tokio::time::Sleep>>,
    _permit: OwnedSemaphorePermit,
}

impl EventBody {
    pub(crate) fn new(
        store: &Store,
        slots: Arc<Semaphore>,
        mut closing: watch::Receiver<bool>,
    ) -> Option<Self> {
        let permit = slots.try_acquire_owned().ok()?;
        Some(Self {
            pending: receive(store.subscribe()),
            closing: Box::pin(async move {
                let _ = closing.wait_for(|closing| *closing).await;
            }),
            initial: true,
            deadline: Box::pin(tokio::time::sleep(Duration::from_secs(25))),
            _permit: permit,
        })
    }
}

impl Body for EventBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.deadline.as_mut().poll(cx).is_ready() || self.closing.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        if self.initial {
            self.initial = false;
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(
                b"retry: 1000\nevent: resync\ndata: {}\n\n",
            )))));
        }
        match self.pending.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready((frame, receiver)) => {
                self.pending = receive(receiver);
                Poll::Ready(frame.map(|bytes| Ok(Frame::data(bytes))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn bounded_streams_resync_on_lag_expire_and_release_slots() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("events.sqlite3"))
            .await
            .unwrap();
        let slots = Arc::new(Semaphore::new(1));
        let (close, closing) = watch::channel(false);
        let mut body = EventBody::new(&store, slots.clone(), closing.clone()).unwrap();
        assert!(EventBody::new(&store, slots.clone(), closing.clone()).is_none());
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert!(
            std::str::from_utf8(&first)
                .unwrap()
                .contains("event: resync")
        );
        for n in 0..=callboard_core::store::CHANGE_CAPACITY {
            store.create_board(&format!("Board {n}")).await.unwrap();
        }
        let lag = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(lag, "event: resync\ndata: {}\n\n");
        let board = store.create_board("Latest").await.unwrap();
        let changed = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(
            changed,
            format!(
                "event: change\ndata: {{\"resource\":\"board\",\"id\":{}}}\n\n",
                board.id
            )
        );
        body.deadline.as_mut().reset(tokio::time::Instant::now());
        assert!(body.frame().await.is_none());
        drop(body);
        let mut body = EventBody::new(&store, slots.clone(), closing).unwrap();
        body.frame().await.unwrap().unwrap();
        close.send(true).unwrap();
        assert!(body.frame().await.is_none(), "closing ends the stream");
        drop(body);
        assert_eq!(slots.available_permits(), 1);
        store.close().await;
    }
}
