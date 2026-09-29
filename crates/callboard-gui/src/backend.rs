//! Read-only service access. Never opens SQLite; auto-start uses the shared
//! client lifecycle, so at most one detached service runs per data directory.
use callboard::{client, lifecycle::Paths};
use callboard_core::{
    layout::NamedLayout,
    store::{BoardContents, BoardInfo, Feed, FeedInfo},
};
use serde::de::DeserializeOwned;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// Concurrent target fetches per batch. Each is its own Unix connection.
const MAX_CONCURRENT: usize = 8;

/// What a panel shows. `Archive` is the deleted-board archive, which is not a
/// layout target (DESIGN.md §6.4) and so is never persisted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    Feed(String),
    Board(i64),
    Archive,
}

pub enum Contents {
    Feed(Feed),
    Board(BoardContents),
    /// The feed or board no longer exists; its panel stays as a placeholder.
    Missing,
}

/// A resource list, refetched independently of the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum List {
    Feeds,
    Boards,
    Layouts,
}

impl List {
    pub const ALL: [List; 3] = [List::Feeds, List::Boards, List::Layouts];
}

pub enum ListData {
    Feeds(Vec<FeedInfo>),
    Boards(Vec<BoardInfo>),
    Layouts(Vec<NamedLayout>),
}

/// One batch of refetches. The UI keeps at most one batch in flight.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    pub lists: BTreeSet<List>,
    pub targets: Vec<Target>,
}

pub struct Fetched {
    pub lists: Vec<(List, Result<ListData, String>)>,
    pub targets: Vec<(Target, Result<Contents, String>)>,
}

enum Failure {
    /// The service could not be reached or started. Later requests in the same
    /// batch would fail the same way (and retry auto-start), so they are skipped.
    Unreachable(String),
    /// A 404 for the route itself, not a missing resource: the service does
    /// not serve this path (a different version).
    UnknownRoute(String),
    Response(String),
}

impl Failure {
    fn message(&self) -> String {
        match self {
            Self::Unreachable(m) | Self::Response(m) => m.clone(),
            Self::UnknownRoute(path) => {
                format!(
                    "The service does not serve {path}; check that callboard and callboard-gui match"
                )
            }
        }
    }
}

/// `Ok(None)` only for a resource-level 404 ("feed not found", "board ...
/// not found"); the service's generic `unknown resource` 404 is an error.
async fn get<T: DeserializeOwned>(
    paths: &Paths,
    auto: Option<&Path>,
    resource: &str,
) -> Result<Option<T>, Failure> {
    let (status, body) = client::request(paths, "GET", resource, vec![], auto)
        .await
        .map_err(|e| Failure::Unreachable(e.to_string()))?;
    if status.as_u16() == 404 {
        let message = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error")?.as_str().map(str::to_owned));
        return match message.as_deref() {
            Some(m) if m != "unknown resource" => Ok(None),
            _ => Err(Failure::UnknownRoute(resource.to_owned())),
        };
    }
    if !status.is_success() {
        return Err(Failure::Response(format!(
            "HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| Failure::Response(format!("Invalid service response: {e}")))
}

async fn required<T: DeserializeOwned>(
    paths: &Paths,
    auto: Option<&Path>,
    resource: &str,
) -> Result<T, Failure> {
    get(paths, auto, resource)
        .await?
        .ok_or_else(|| Failure::UnknownRoute(resource.to_owned()))
}

async fn list(paths: &Paths, auto: Option<&Path>, list: List) -> Result<ListData, Failure> {
    Ok(match list {
        List::Feeds => ListData::Feeds(required(paths, auto, "/feeds").await?),
        List::Boards => ListData::Boards(required(paths, auto, "/boards").await?),
        // A service predating layouts has no route; browse without them.
        List::Layouts => ListData::Layouts(match get(paths, auto, "/layouts").await {
            Ok(layouts) => layouts.unwrap_or_default(),
            Err(Failure::UnknownRoute(_)) => Vec::new(),
            Err(e) => return Err(e),
        }),
    })
}

async fn contents(
    paths: &Paths,
    auto: Option<&Path>,
    target: &Target,
) -> Result<Contents, Failure> {
    let found = match target {
        Target::Feed(name) => {
            if callboard_core::feed::validate_feed_name(name).is_err() {
                return Ok(Contents::Missing);
            }
            get(paths, auto, &format!("/feeds/{name}"))
                .await?
                .map(Contents::Feed)
        }
        Target::Board(id) if *id > 1 => get(paths, auto, &format!("/boards/{id}"))
            .await?
            .map(Contents::Board),
        Target::Board(_) => None,
        Target::Archive => Some(Contents::Board(required(paths, auto, "/archive").await?)),
    };
    Ok(found.unwrap_or(Contents::Missing))
}

fn unreachable_message(failure: &Failure) -> Option<String> {
    matches!(failure, Failure::Unreachable(_)).then(|| failure.message())
}

/// Fetch a batch. Lists are fetched together first; targets follow with
/// bounded concurrency. The first request in a batch acts as a probe: if the
/// service is unreachable, the rest fail at once instead of each retrying
/// the connection (and auto-start) in parallel.
pub async fn fetch(paths: &Paths, auto: Option<&Path>, request: &Request) -> Fetched {
    let mut unreachable = None;
    let lists_to_fetch: Vec<List> = request.lists.iter().copied().collect();
    let mut lists = Vec::new();
    // Probe with the first list, then fetch the others concurrently.
    if let Some((first, rest)) = lists_to_fetch.split_first() {
        let result = list(paths, auto, *first).await;
        unreachable = result.as_ref().err().and_then(unreachable_message);
        lists.push((*first, result.map_err(|f| f.message())));
        let (a, b) = (rest.first().copied(), rest.get(1).copied());
        let run = |l: Option<List>, skip: bool| async move {
            match l {
                Some(l) if !skip => Some((l, list(paths, auto, l).await)),
                _ => None,
            }
        };
        let skip = unreachable.is_some();
        let (ra, rb) = tokio::join!(run(a, skip), run(b, skip));
        for result in [ra, rb].into_iter().flatten() {
            lists.push((result.0, result.1.map_err(|f| f.message())));
        }
        if let Some(message) = &unreachable {
            for l in rest {
                lists.push((*l, Err(message.clone())));
            }
        }
    }
    let mut results: Vec<Option<Result<Contents, String>>> =
        (0..request.targets.len()).map(|_| None).collect();
    let mut pending = request.targets.iter().cloned().enumerate();
    let mut set = tokio::task::JoinSet::new();
    let mut probed = !request.lists.is_empty();
    let auto: Option<PathBuf> = auto.map(Path::to_path_buf);
    loop {
        let limit = if probed { MAX_CONCURRENT } else { 1 };
        while set.len() < limit {
            let Some((index, target)) = pending.next() else {
                break;
            };
            if let Some(message) = &unreachable {
                results[index] = Some(Err(message.clone()));
                continue;
            }
            let (paths, auto) = (paths.clone(), auto.clone());
            set.spawn(async move { (index, contents(&paths, auto.as_deref(), &target).await) });
        }
        let Some(joined) = set.join_next().await else {
            break;
        };
        probed = true;
        let (index, result) = joined.expect("target fetch task panicked");
        if unreachable.is_none() {
            unreachable = result.as_ref().err().and_then(unreachable_message);
        }
        results[index] = Some(result.map_err(|f| f.message()));
    }
    let targets = request
        .targets
        .iter()
        .cloned()
        .zip(results)
        .map(|(t, r)| (t, r.unwrap_or_else(|| Err("not fetched".into()))))
        .collect();
    Fetched { lists, targets }
}

/// Percent-encode one URI path segment (layout names are arbitrary text).
fn encode_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Create or replace a named layout (`PUT /layouts/{name}`). The only write
/// the GUI makes; feeds, boards, and items stay read-only.
pub async fn save_layout(
    paths: &Paths,
    auto: Option<&Path>,
    name: &str,
    tree: &callboard_core::layout::Panel,
) -> Result<NamedLayout, String> {
    let body = serde_json::to_vec(&callboard_core::layout::Layout { tree: tree.clone() })
        .map_err(|e| e.to_string())?;
    let resource = format!("/layouts/{}", encode_segment(name));
    let (status, bytes) = client::request(paths, "PUT", &resource, body, auto)
        .await
        .map_err(|e| e.to_string())?;
    if !status.is_success() {
        let message = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("error")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
        return Err(format!("HTTP {status}: {message}"));
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("Invalid service response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Service;
    use serde_json::json;

    fn all(targets: &[Target]) -> Request {
        Request {
            lists: List::ALL.into_iter().collect(),
            targets: targets.to_vec(),
        }
    }

    struct Loaded {
        feeds: Vec<FeedInfo>,
        boards: Vec<BoardInfo>,
        layouts: Vec<NamedLayout>,
    }

    fn loaded(fetched: Fetched) -> Loaded {
        let mut out = Loaded {
            feeds: vec![],
            boards: vec![],
            layouts: vec![],
        };
        assert_eq!(fetched.lists.len(), 3);
        for (_, result) in fetched.lists {
            match result.unwrap() {
                ListData::Feeds(f) => out.feeds = f,
                ListData::Boards(b) => out.boards = b,
                ListData::Layouts(l) => out.layouts = l,
            }
        }
        out
    }

    #[tokio::test]
    async fn reads_authenticated_service_and_retains_deleted_target_placeholders() {
        let service = Service::new();
        let unreachable = fetch(&service.paths, None, &all(&[Target::Archive])).await;
        assert_eq!(unreachable.lists.len(), 3);
        assert!(unreachable.lists.iter().all(|(_, r)| r.is_err()));
        assert!(unreachable.targets[0].1.is_err());
        assert!(!service.paths.socket().exists());
        let running = service.start().await;
        running
            .send(
                "PUT",
                "/feeds/work",
                json!({"items":[{"key":"a","title":"Original"}]}),
            )
            .await;
        let id = running
            .send("POST", "/boards", json!({"name":"Inbox"}))
            .await["id"]
            .as_i64()
            .unwrap();
        running
            .send(
                "POST",
                "/feeds/work/items/a/promote",
                json!({"board_id":id,"kind":"todo"}),
            )
            .await;
        running
            .send(
                "PUT",
                "/feeds/work",
                json!({"items":[{"key":"a","title":"Current"}]}),
            )
            .await;
        running
            .send(
                "PUT",
                "/layouts/Day",
                json!({"tree":{"kind":"board","id":id}}),
            )
            .await;
        let view = fetch(
            &service.paths,
            None,
            &all(&[Target::Board(id), Target::Feed("work".into())]),
        )
        .await;
        let targets = view.targets;
        let lists = loaded(Fetched {
            lists: view.lists,
            targets: vec![],
        });
        assert_eq!(lists.feeds.len(), 1);
        assert_eq!(lists.boards.len(), 1);
        assert_eq!(lists.layouts[0].name, "Day");
        let Ok(Contents::Board(board)) = &targets[0].1 else {
            panic!("missing board")
        };
        assert_eq!(board.todos[0].item.title, "Original");
        assert!(
            matches!(&board.todos[0].resolved_reference,Some(callboard_core::store::ResolvedReference::Live {item}) if item.title == "Current")
        );
        assert!(
            matches!(&targets[1].1, Ok(Contents::Feed(feed)) if feed.items[0].title == "Current")
        );
        running
            .send(
                "DELETE",
                &format!("/boards/{id}"),
                json!({"archive_contents":true}),
            )
            .await;
        running.send("DELETE", "/feeds/work", json!(null)).await;
        let view = fetch(
            &service.paths,
            None,
            &all(&[
                Target::Board(id),
                Target::Feed("work".into()),
                Target::Archive,
            ]),
        )
        .await;
        let targets = view.targets;
        let lists = loaded(Fetched {
            lists: view.lists,
            targets: vec![],
        });
        assert!(lists.boards.is_empty() && lists.feeds.is_empty());
        // Layouts are unchanged by target deletion (DESIGN.md §6.4).
        assert_eq!(lists.layouts.len(), 1);
        assert!(matches!(targets[0].1, Ok(Contents::Missing)));
        assert!(matches!(targets[1].1, Ok(Contents::Missing)));
        let Ok(Contents::Board(archive)) = &targets[2].1 else {
            panic!("missing archive")
        };
        assert!(matches!(
            archive.todos[0].resolved_reference,
            Some(callboard_core::store::ResolvedReference::SourceGone)
        ));
        running.stop().await;
        let after = fetch(&service.paths, None, &all(&[])).await;
        assert!(after.lists.iter().all(|(_, r)| r.is_err()));
    }

    #[tokio::test]
    async fn a_service_without_layouts_still_lists_feeds_and_boards() {
        let service = Service::new();
        let fake = crate::testing::serve_fake(&service.paths, |path| match path {
            "/feeds" => (
                200,
                r#"[{"name":"work","title":"Work","source_url":null,"stale_after":null,"last_submitted_at_ms":0,"error":null}]"#,
            ),
            "/boards" => (200, r#"[{"id":2,"name":"Inbox"}]"#),
            _ => (404, r#"{"error":"unknown resource"}"#),
        })
        .await;
        let view = fetch(&service.paths, None, &all(&[])).await;
        let lists = loaded(view);
        assert_eq!(lists.feeds[0].name, "work");
        assert_eq!(lists.boards[0].name, "Inbox");
        assert!(lists.layouts.is_empty());
        fake.abort();
    }

    #[tokio::test]
    async fn an_unknown_route_404_is_an_error_not_a_deleted_target() {
        let service = Service::new();
        let fake = crate::testing::serve_fake(&service.paths, |path| match path {
            "/feeds/gone" => (404, r#"{"error":"feed not found"}"#),
            _ => (404, r#"{"error":"unknown resource"}"#),
        })
        .await;
        let view = fetch(
            &service.paths,
            None,
            &Request {
                lists: BTreeSet::new(),
                targets: vec![Target::Feed("gone".into()), Target::Board(2)],
            },
        )
        .await;
        assert!(matches!(view.targets[0].1, Ok(Contents::Missing)));
        let Err(error) = &view.targets[1].1 else {
            panic!("a route mismatch must not look like a deleted board")
        };
        assert!(error.contains("/boards/2"), "{error}");
        fake.abort();
    }

    #[tokio::test]
    async fn many_targets_are_fetched_concurrently_and_returned_in_request_order() {
        let service = Service::new();
        let running = service.start().await;
        let names: Vec<String> = (0..20).map(|n| format!("feed-{n:02}")).collect();
        for name in &names {
            running
                .send(
                    "PUT",
                    &format!("/feeds/{name}"),
                    json!({"items":[{"key":"k","title":name}]}),
                )
                .await;
        }
        let targets: Vec<Target> = names.iter().rev().cloned().map(Target::Feed).collect();
        let view = fetch(
            &service.paths,
            None,
            &Request {
                lists: BTreeSet::new(),
                targets: targets.clone(),
            },
        )
        .await;
        for ((target, result), expected) in view.targets.iter().zip(&targets) {
            assert_eq!(target, expected);
            let (Target::Feed(name), Ok(Contents::Feed(feed))) = (target, result) else {
                panic!("feed not loaded")
            };
            assert_eq!(&feed.items[0].title, name);
        }
        running.stop().await;
    }

    #[tokio::test]
    async fn saves_layouts_under_encoded_names_and_reports_rejections() {
        let service = Service::new();
        let running = service.start().await;
        let tree: callboard_core::layout::Panel =
            serde_json::from_value(json!({"kind":"feed","name":"reviews"})).unwrap();
        let name = "Day / night ✓ 100%";
        let saved = save_layout(&service.paths, None, name, &tree)
            .await
            .unwrap();
        assert_eq!(saved.name, name);
        assert_eq!(saved.tree, tree);
        let view = fetch(&service.paths, None, &all(&[])).await;
        assert_eq!(loaded(view).layouts[0].name, name);
        let invalid: callboard_core::layout::Panel =
            serde_json::from_value(json!({"kind":"board","id":1})).unwrap();
        let error = save_layout(&service.paths, None, "Bad", &invalid)
            .await
            .unwrap_err();
        assert!(error.starts_with("HTTP 400"), "{error}");
        running.stop().await;
    }
}
