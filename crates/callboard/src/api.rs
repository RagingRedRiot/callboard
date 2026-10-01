//! Route dispatch for the feature API (DESIGN.md §8.1). Route handlers live
//! in `src/routes/`, one file per feature area, registered by `build.rs`.
//! This module owns what they must not: path matching and capture
//! validation, body size and time limits, and the rule that no route may
//! overlap another or a service endpoint, so adding a route file can never
//! change what an existing route does.
use callboard_core::{
    feed::{MAX_SNAPSHOT_BYTES, validate_feed_name},
    store::{Store, StoreError},
};
use http_body_util::{BodyExt, Full, Limited, combinators::UnsyncBoxBody};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{convert::Infallible, future::Future, pin::Pin, sync::OnceLock, time::Duration};

// Route files are append-only and need no review to add (docs/merge-checks.md),
// so they may not use unsafe code, which in Rust 2024 also rules out symbol
// interposition (`no_mangle`, `export_name`) and a replacement allocator.
#[forbid(unsafe_code)]
mod routes {
    include!(concat!(env!("OUT_DIR"), "/routes.rs"));
}

pub(crate) type Reply = Response<UnsyncBoxBody<Bytes, Infallible>>;
pub(crate) type Handler = fn(Call) -> Pin<Box<dyn Future<Output = Reply> + Send>>;

/// Bodies of small edits: failure reports, feed item patches, promotion.
pub(crate) const SMALL_BODY: usize = 16 * 1024;
/// Bodies of board, item, layout, and preference requests.
pub(crate) const BODY: usize = 64 * 1024;
/// No route may accept more than a feed snapshot.
pub(crate) const MAX_BODY: usize = MAX_SNAPSHOT_BYTES;
/// Paths served by `server.rs` itself; no route may overlap them.
const RESERVED: [&str; 3] = ["/health", "/events", "/service/upgrade"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verb {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Verb {
    fn of(method: &Method) -> Option<Self> {
        Some(match *method {
            Method::GET => Self::Get,
            Method::POST => Self::Post,
            Method::PUT => Self::Put,
            Method::PATCH => Self::Patch,
            Method::DELETE => Self::Delete,
            _ => return None,
        })
    }
    fn name(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// One method and path pattern. Pattern segments are literals or captures:
/// `{feed}` (a valid feed name), `{id}` (an i64), and `{key}` or `{name}`
/// (any segment, percent-decoded exactly once).
pub(crate) struct Route {
    pub verb: Verb,
    pub path: &'static str,
    pub handler: Handler,
}

/// `route!(GET "/feeds/{feed}" => handler)`, where `handler` is an
/// `async fn(Call) -> Reply`.
macro_rules! route {
    ($verb:ident $path:literal => $handler:path) => {
        $crate::api::Route {
            verb: $crate::api::route!(@verb $verb),
            path: $path,
            handler: |call| Box::pin($handler(call)),
        }
    };
    (@verb GET) => { $crate::api::Verb::Get };
    (@verb POST) => { $crate::api::Verb::Post };
    (@verb PUT) => { $crate::api::Verb::Put };
    (@verb PATCH) => { $crate::api::Verb::Patch };
    (@verb DELETE) => { $crate::api::Verb::Delete };
}
pub(crate) use route;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capture {
    Feed,
    Id,
    Key,
    Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(&'static str),
    Capture(Capture),
}

fn parse_pattern(path: &'static str) -> Result<Vec<Segment>, String> {
    let rest = path
        .strip_prefix('/')
        .ok_or_else(|| format!("route {path} must start with '/'"))?;
    rest.split('/')
        .map(|segment| match segment {
            "{feed}" => Ok(Segment::Capture(Capture::Feed)),
            "{id}" => Ok(Segment::Capture(Capture::Id)),
            "{key}" => Ok(Segment::Capture(Capture::Key)),
            "{name}" => Ok(Segment::Capture(Capture::Name)),
            "" => Err(format!("route {path} has an empty segment")),
            s if s.contains(['{', '}', '%']) => {
                Err(format!("route {path} has an unknown capture {s}"))
            }
            s => Ok(Segment::Literal(s)),
        })
        .collect()
}

/// Two patterns overlap when some path could match both. Captures are
/// treated as matching anything, so a literal beside an existing capture
/// (a feed named like a new route) counts too.
fn overlap(a: &[Segment], b: &[Segment]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|pair| match pair {
            (Segment::Literal(x), Segment::Literal(y)) => x == y,
            _ => true,
        })
}

struct Entry {
    verb: Verb,
    path: &'static str,
    pattern: Vec<Segment>,
    handler: Handler,
}

pub(crate) struct Table(Vec<Entry>);

impl Table {
    fn build(groups: &[&[Route]]) -> Result<Self, String> {
        let reserved = RESERVED
            .iter()
            .map(|path| Ok((*path, parse_pattern(path)?)))
            .collect::<Result<Vec<_>, String>>()?;
        let mut entries: Vec<Entry> = Vec::new();
        for route in groups.iter().flat_map(|group| group.iter()) {
            let pattern = parse_pattern(route.path)?;
            if let Some((path, _)) = reserved.iter().find(|(_, r)| overlap(r, &pattern)) {
                return Err(format!(
                    "route {} overlaps service endpoint {path}",
                    route.path
                ));
            }
            if let Some(other) = entries
                .iter()
                .find(|e| e.verb == route.verb && overlap(&e.pattern, &pattern))
            {
                return Err(format!(
                    "route {} {} overlaps {} {}",
                    route.verb.name(),
                    route.path,
                    other.verb.name(),
                    other.path
                ));
            }
            entries.push(Entry {
                verb: route.verb,
                path: route.path,
                pattern,
                handler: route.handler,
            });
        }
        Ok(Self(entries))
    }

    /// Match `segments` against `entry`; `None` if the shape or a structural
    /// capture (`{feed}`, `{id}`) does not fit.
    fn bind(entry: &Entry, segments: &[&str]) -> Option<Raw> {
        if entry.pattern.len() != segments.len() {
            return None;
        }
        let mut raw = Raw::default();
        for (part, segment) in entry.pattern.iter().zip(segments) {
            match part {
                Segment::Literal(literal) if literal == segment => (),
                Segment::Literal(_) => return None,
                Segment::Capture(Capture::Feed) => {
                    validate_feed_name(segment).ok()?;
                    raw.feed = Some(segment.to_string());
                }
                Segment::Capture(Capture::Id) => raw.id = Some(segment.parse().ok()?),
                Segment::Capture(Capture::Key) => raw.key = Some(segment.to_string()),
                Segment::Capture(Capture::Name) => raw.name = Some(segment.to_string()),
            }
        }
        Some(raw)
    }
}

/// The table every request is matched against, built once.
pub(crate) fn table() -> Result<&'static Table, String> {
    static TABLE: OnceLock<Result<Table, String>> = OnceLock::new();
    TABLE
        .get_or_init(|| Table::build(routes::ALL))
        .as_ref()
        .map_err(Clone::clone)
}

/// Every registered route as (method, pattern), for contract tests.
pub fn list() -> Result<Vec<(&'static str, &'static str)>, String> {
    Ok(table()?
        .0
        .iter()
        .map(|entry| (entry.verb.name(), entry.path))
        .collect())
}

#[derive(Default)]
struct Raw {
    feed: Option<String>,
    id: Option<i64>,
    key: Option<String>,
    name: Option<String>,
}

/// A request refused before the handler's work, such as an oversized or
/// malformed body. `.into()` gives the reply to send.
pub(crate) struct Rejected(Box<Reply>);

impl From<Rejected> for Reply {
    fn from(rejected: Rejected) -> Self {
        *rejected.0
    }
}

fn reject(status: StatusCode, message: impl ToString) -> Rejected {
    Rejected(Box::new(error(status, message)))
}

/// A matched request, as a route handler receives it.
pub(crate) struct Call {
    pub store: Store,
    feed: Option<String>,
    id: Option<i64>,
    key: Option<String>,
    name: Option<String>,
    headers: HeaderMap,
    body: Option<Incoming>,
}

impl Call {
    /// The `{feed}` capture. Panics if the route declares none.
    pub fn feed(&self) -> &str {
        self.feed.as_deref().expect("route declares {feed}")
    }
    /// The `{id}` capture. Panics if the route declares none.
    pub fn id(&self) -> i64 {
        self.id.expect("route declares {id}")
    }
    /// The decoded `{key}` capture. Panics if the route declares none.
    pub fn key(&self) -> &str {
        self.key.as_deref().expect("route declares {key}")
    }
    /// The decoded `{name}` capture. Panics if the route declares none.
    pub fn name(&self) -> &str {
        self.name.as_deref().expect("route declares {name}")
    }

    /// Read the body as JSON, at most `limit` bytes (capped at [`MAX_BODY`])
    /// within 10 seconds.
    pub async fn json<T: for<'de> Deserialize<'de>>(
        &mut self,
        limit: usize,
    ) -> Result<T, Rejected> {
        let body = self.bytes(limit).await?;
        serde_json::from_slice(&body).map_err(|e| reject(StatusCode::BAD_REQUEST, e))
    }

    /// As [`Call::json`], but an empty body is `T::default()`.
    pub async fn json_or_default<T: for<'de> Deserialize<'de> + Default>(
        &mut self,
        limit: usize,
    ) -> Result<T, Rejected> {
        let body = self.bytes(limit).await?;
        if body.is_empty() {
            return Ok(T::default());
        }
        serde_json::from_slice(&body).map_err(|e| reject(StatusCode::BAD_REQUEST, e))
    }

    async fn bytes(&mut self, limit: usize) -> Result<Bytes, Rejected> {
        let limit = limit.min(MAX_BODY);
        let too_large = || reject(StatusCode::PAYLOAD_TOO_LARGE, "request body exceeds limit");
        if self
            .headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|n| n > limit as u64)
        {
            return Err(too_large());
        }
        let Some(body) = self.body.take() else {
            return Err(reject(StatusCode::BAD_REQUEST, "request body already read"));
        };
        match tokio::time::timeout(Duration::from_secs(10), Limited::new(body, limit).collect())
            .await
        {
            Err(_) => Err(reject(
                StatusCode::REQUEST_TIMEOUT,
                "request body timed out",
            )),
            Ok(Err(e)) if e.is::<http_body_util::LengthLimitError>() => Err(too_large()),
            Ok(Err(_)) => Err(reject(StatusCode::BAD_REQUEST, "invalid request body")),
            Ok(Ok(v)) => Ok(v.to_bytes()),
        }
    }
}

/// Route `request` through the table: 404 for an unknown path, 405 for a
/// known path with another method, 400 for an undecodable capture.
pub(crate) async fn dispatch(request: Request<Incoming>, store: &Store) -> Reply {
    let table = match table() {
        Ok(table) => table,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let path = request.uri().path().to_owned();
    let Some(segments) = path
        .strip_prefix('/')
        .map(|p| p.split('/').collect::<Vec<_>>())
    else {
        return error(StatusCode::NOT_FOUND, "unknown resource");
    };
    let verb = Verb::of(request.method());
    let mut path_known = false;
    for entry in &table.0 {
        let Some(raw) = Table::bind(entry, &segments) else {
            continue;
        };
        path_known = true;
        if Some(entry.verb) != verb {
            continue;
        }
        let decode = |value: Option<String>| value.map(|v| decode_path_segment(&v)).transpose();
        let (key, name) = match (decode(raw.key), decode(raw.name)) {
            (Ok(key), Ok(name)) => (key, name),
            (Err(e), _) | (_, Err(e)) => return error(StatusCode::BAD_REQUEST, e),
        };
        let (parts, body) = request.into_parts();
        let call = Call {
            store: store.clone(),
            feed: raw.feed,
            id: raw.id,
            key,
            name,
            headers: parts.headers,
            body: Some(body),
        };
        return (entry.handler)(call).await;
    }
    if path_known {
        error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
    } else {
        error(StatusCode::NOT_FOUND, "unknown resource")
    }
}

pub(crate) fn reply(status: StatusCode, value: impl Serialize) -> Reply {
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

pub(crate) fn error(status: StatusCode, message: impl ToString) -> Reply {
    reply(status, json!({"error":message.to_string()}))
}

/// A store result as a reply: `status` with the value, or the error's status.
pub(crate) fn stored<T: Serialize>(status: StatusCode, result: Result<T, StoreError>) -> Reply {
    match result {
        Ok(value) => reply(status, value),
        Err(e) => storage_error(e),
    }
}

/// `{"ok":true}` for a store operation without a result.
pub(crate) fn done(result: Result<(), StoreError>) -> Reply {
    stored(StatusCode::OK, result.map(|()| json!({"ok":true})))
}

pub(crate) fn storage_error(e: StoreError) -> Reply {
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn nothing(_: Call) -> Reply {
        error(StatusCode::NO_CONTENT, "")
    }
    const FEED: &[Route] = &[route!(GET "/feeds/{feed}" => nothing)];

    #[test]
    fn registered_routes_build_without_overlaps() {
        table().unwrap();
    }

    #[test]
    fn a_new_route_cannot_shadow_an_existing_one_or_a_service_endpoint() {
        for shadow in [
            &[route!(GET "/feeds/special" => nothing)][..],
            &[route!(GET "/feeds/{name}" => nothing)],
            &[route!(GET "/{name}/{key}" => nothing)],
        ] {
            assert!(Table::build(&[FEED, shadow]).is_err());
        }
        for reserved in [
            &[route!(POST "/health" => nothing)][..],
            &[route!(GET "/service/{name}" => nothing)],
            &[route!(DELETE "/{name}" => nothing)],
        ] {
            assert!(Table::build(&[reserved]).is_err());
        }
        // Another method on the same path, or a longer path, is a new route.
        let other = &[
            route!(DELETE "/feeds/{feed}" => nothing),
            route!(GET "/feeds/{feed}/history" => nothing),
        ][..];
        assert!(Table::build(&[FEED, other]).is_ok());
    }

    #[test]
    fn malformed_patterns_are_rejected() {
        for path in ["feeds", "/feeds//x", "/feeds/{bogus}", "/feeds/%41"] {
            let bad = [Route {
                verb: Verb::Get,
                path,
                handler: |call| Box::pin(nothing(call)),
            }];
            assert!(Table::build(&[&bad]).is_err(), "{path}");
        }
    }

    #[test]
    fn captures_validate_structure() {
        let entry = |path| Entry {
            verb: Verb::Get,
            path,
            pattern: parse_pattern(path).unwrap(),
            handler: |call| Box::pin(nothing(call)),
        };
        let feed = entry("/feeds/{feed}");
        assert!(Table::bind(&feed, &["feeds", "reviews"]).is_some());
        assert!(Table::bind(&feed, &["feeds", ""]).is_none());
        let board = entry("/boards/{id}");
        assert!(Table::bind(&board, &["boards", "12"]).is_some());
        assert!(Table::bind(&board, &["boards", "x"]).is_none());
        assert!(decode_path_segment("a%2Fb+c").unwrap() == "a/b+c");
        assert!(decode_path_segment("%zz").is_err());
    }
}
