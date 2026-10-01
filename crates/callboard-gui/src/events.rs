//! `GET /events` subscription (DESIGN.md §8.2): parse change notices and keep
//! reconnecting. Every connection starts with a resync. The scheduler refetches
//! on it except after a routine rotation (DESIGN.md §6.5), where the reconnect
//! gap is milliseconds and a periodic full refetch bounds what it could miss.
use callboard::lifecycle::Paths;
use callboard_core::store::Change;
use std::time::{Duration, Instant};

/// A line longer than this is not a valid notice; the stream is dropped.
const MAX_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Discard cached assumptions and refetch everything visible.
    Resync,
    Change(Change),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    Connected,
    /// The connected service's build identity (DESIGN.md §7.4), sent right
    /// after `Connected` when the service reports one.
    Build(String),
    Notice(Notice),
    /// `None` for an orderly end of stream (the service rotates streams).
    Disconnected(Option<String>),
}

/// Incremental `text/event-stream` parser for the fields the service sends.
#[derive(Default)]
pub struct Parser {
    line: Vec<u8>,
    event: String,
    data: String,
    retry: Option<Duration>,
}

impl Parser {
    /// The most recent `retry:` interval, if the service sent one.
    pub fn retry(&self) -> Option<Duration> {
        self.retry
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Notice>, String> {
        let mut notices = Vec::new();
        for &byte in bytes {
            if byte != b'\n' {
                if self.line.len() >= MAX_LINE {
                    return Err("event stream line exceeds 64 KiB".into());
                }
                self.line.push(byte);
                continue;
            }
            let mut line = std::mem::take(&mut self.line);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8(line).map_err(|_| "event stream is not UTF-8")?;
            if line.is_empty() {
                notices.extend(self.dispatch());
                continue;
            }
            if line.starts_with(':') {
                continue; // Heartbeat comment.
            }
            let (field, value) = line.split_once(':').unwrap_or((&line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.event = value.to_owned(),
                "data" => {
                    if !self.data.is_empty() {
                        self.data.push('\n');
                    }
                    self.data.push_str(value);
                }
                "retry" => {
                    if let Ok(ms) = value.parse::<u64>() {
                        self.retry = Some(Duration::from_millis(ms));
                    }
                }
                _ => (),
            }
        }
        Ok(notices)
    }

    fn dispatch(&mut self) -> Option<Notice> {
        let event = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        match event.as_str() {
            "resync" => Some(Notice::Resync),
            // An unreadable notice may have named anything; refetch everything.
            "change" => Some(
                serde_json::from_str(&data)
                    .map(Notice::Change)
                    .unwrap_or(Notice::Resync),
            ),
            _ => None,
        }
    }
}

/// Reconnect delays. A stream that stayed up for at least the base interval
/// was healthy: reconnect at once. Otherwise back off exponentially to `max`.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    failures: u32,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            failures: 0,
        }
    }

    /// Adopt the service's `retry:` hint, clamped to a sane range.
    pub fn set_base(&mut self, retry: Duration) {
        // Not `clamp`, which panics if a caller's `max` is below the floor.
        self.base = retry.max(Duration::from_millis(100)).min(self.max);
    }

    pub fn failed(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        self.base
            .saturating_mul(2u32.saturating_pow(self.failures - 1))
            .min(self.max)
    }

    pub fn ended(&mut self, lasted: Duration) -> Duration {
        if lasted >= self.base {
            self.failures = 0;
            Duration::ZERO
        } else {
            self.failed()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Policy {
    /// The service sends a heartbeat every 10 seconds; silence longer than
    /// this means the connection is dead.
    pub idle_timeout: Duration,
    pub base: Duration,
    pub max: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(30),
            base: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

enum End {
    Stopped,
    Closed,
}

async fn session(
    paths: &Paths,
    policy: &Policy,
    parser: &mut Parser,
    sink: &mut impl FnMut(Signal) -> bool,
) -> Result<End, String> {
    // Never auto-start from here; the fetch path owns service startup.
    let (status, mut stream) = callboard::client::stream(paths, "/events", None)
        .await
        .map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("event stream unavailable: HTTP {status}"));
    }
    if !sink(Signal::Connected) {
        return Ok(End::Stopped);
    }
    if let Some(build) = stream.header("x-callboard-build")
        && !sink(Signal::Build(build.to_owned()))
    {
        return Ok(End::Stopped);
    }
    loop {
        let chunk = tokio::time::timeout(policy.idle_timeout, stream.chunk())
            .await
            .map_err(|_| "event stream went silent".to_string())?
            .map_err(|e| e.to_string())?;
        let Some(chunk) = chunk else {
            return Ok(End::Closed);
        };
        for notice in parser.push(&chunk)? {
            if !sink(Signal::Notice(notice)) {
                return Ok(End::Stopped);
            }
        }
    }
}

/// Subscribe until `sink` returns false (its receiver is gone).
pub async fn run(paths: &Paths, policy: Policy, mut sink: impl FnMut(Signal) -> bool) {
    let mut backoff = Backoff::new(policy.base, policy.max);
    loop {
        let mut parser = Parser::default();
        let started = Instant::now();
        let result = session(paths, &policy, &mut parser, &mut sink).await;
        if let Some(retry) = parser.retry() {
            backoff.set_base(retry);
        }
        let (delay, reason) = match result {
            Ok(End::Stopped) => return,
            Ok(End::Closed) => (backoff.ended(started.elapsed()), None),
            Err(e) if parser.retry().is_some() => (backoff.ended(started.elapsed()), Some(e)),
            Err(e) => (backoff.failed(), Some(e)),
        };
        if !sink(Signal::Disconnected(reason)) {
            return;
        }
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Service;
    use serde_json::json;

    #[test]
    fn parser_handles_split_chunks_comments_and_unknown_events() {
        let mut parser = Parser::default();
        let stream = b"retry: 1000\nevent: resync\ndata: {}\n\n: heartbeat\n\n\
            event: change\r\ndata: {\"resource\":\"feed\",\"name\":\"reviews\"}\r\n\r\n\
            event: other\ndata: x\n\nevent: change\ndata: nonsense\n\n";
        let mut notices = Vec::new();
        for chunk in stream.chunks(7) {
            notices.extend(parser.push(chunk).unwrap());
        }
        assert_eq!(
            notices,
            vec![
                Notice::Resync,
                Notice::Change(Change::Feed {
                    name: "reviews".into()
                }),
                Notice::Resync,
            ]
        );
        assert_eq!(parser.retry(), Some(Duration::from_secs(1)));
        assert!(parser.push(&vec![b'x'; MAX_LINE + 1]).is_err());
    }

    #[test]
    fn backoff_resets_after_healthy_streams_and_caps_failures() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(8));
        let delays: Vec<_> = (0..5).map(|_| backoff.failed().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 8]);
        assert_eq!(backoff.ended(Duration::from_secs(25)), Duration::ZERO);
        assert_eq!(backoff.failed(), Duration::from_secs(1));
        // A stream that ends immediately is treated as a failure, not a rotation.
        assert_eq!(
            backoff.ended(Duration::from_millis(10)),
            Duration::from_secs(2)
        );
        backoff.set_base(Duration::from_millis(1));
        assert_eq!(backoff.ended(Duration::ZERO), Duration::from_millis(400));
        // A maximum below the 100 ms floor caps the base instead of panicking.
        let mut tight = Backoff::new(Duration::from_millis(10), Duration::from_millis(50));
        tight.set_base(Duration::from_secs(1));
        assert_eq!(tight.failed(), Duration::from_millis(50));
    }

    async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Signal>) -> Signal {
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("event signal")
            .expect("worker running")
    }

    #[tokio::test]
    async fn subscribes_resyncs_and_reconnects_after_service_restart() {
        let service = Service::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let paths = service.paths.clone();
        let policy = Policy {
            idle_timeout: Duration::from_secs(30),
            base: Duration::from_millis(50),
            max: Duration::from_millis(200),
        };
        let worker =
            tokio::spawn(async move { run(&paths, policy, move |s| tx.send(s).is_ok()).await });
        // No service yet: the worker reports failures and keeps retrying.
        assert!(matches!(next(&mut rx).await, Signal::Disconnected(Some(_))));
        let running = service.start().await;
        let mut signal = next(&mut rx).await;
        while signal != Signal::Connected {
            assert!(matches!(signal, Signal::Disconnected(Some(_))));
            signal = next(&mut rx).await;
        }
        assert_eq!(
            next(&mut rx).await,
            Signal::Build(callboard::BUILD.to_owned())
        );
        assert_eq!(next(&mut rx).await, Signal::Notice(Notice::Resync));
        let board = running
            .send("POST", "/boards", json!({"name":"Inbox"}))
            .await;
        let id = board["id"].as_i64().unwrap();
        assert_eq!(
            next(&mut rx).await,
            Signal::Notice(Notice::Change(Change::Board { id }))
        );
        running.stop().await;
        assert!(matches!(next(&mut rx).await, Signal::Disconnected(_)));
        let running = service.start().await;
        let mut signal = next(&mut rx).await;
        while signal != Signal::Connected {
            assert!(matches!(signal, Signal::Disconnected(Some(_))));
            signal = next(&mut rx).await;
        }
        assert!(matches!(next(&mut rx).await, Signal::Build(_)));
        // The new subscription begins with a resync covering the missed gap.
        assert_eq!(next(&mut rx).await, Signal::Notice(Notice::Resync));
        drop(rx);
        running
            .send(
                "PUT",
                "/layouts/Day",
                json!({"view":{"x":0,"y":0},"cards":[]}),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(10), worker)
            .await
            .expect("worker stops once its receiver is gone")
            .unwrap();
        running.stop().await;
    }
}
