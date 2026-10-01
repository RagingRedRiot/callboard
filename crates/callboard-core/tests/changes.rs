//! Feed descriptions, item colors, and change tracking (DESIGN.md §3, §4.3).
use callboard_core::feed::{Snapshot, parse_submission};
use callboard_core::store::{ChangeStatus, ItemChange, RemovedItem, Store};
use serde_json::{Value, json};
use std::time::Duration;

fn snapshot(value: Value) -> Snapshot {
    parse_submission(&serde_json::to_vec(&value).unwrap()).unwrap()
}

fn items(pairs: &[(&str, &str)]) -> Value {
    json!(
        pairs
            .iter()
            .map(|(key, title)| json!({"key": key, "title": title}))
            .collect::<Vec<_>>()
    )
}

async fn status(store: &Store, key: &str) -> Option<ChangeStatus> {
    store.feed("work").await.unwrap().unwrap().changes[key].status
}

async fn new_count(store: &Store) -> usize {
    store.feed_summaries().await.unwrap()[0].new_count
}

#[test]
fn marks_follow_the_window_from_added_and_changed_times() {
    let hour = Some(3_600_000);
    let at = |added, changed| ItemChange {
        added_at_ms: added,
        changed_at_ms: changed,
        status: None,
    };
    let now = 10_000_000;
    // New for the window after it was added, wherever it changed since.
    assert_eq!(
        at(now - 1000, now - 500).status_at(hour, now),
        Some((ChangeStatus::New, now - 1000 + 3_600_000))
    );
    // Then updated for the window after its last change.
    assert_eq!(
        at(now - 5_000_000, now - 1000).status_at(hour, now),
        Some((ChangeStatus::Updated, now - 1000 + 3_600_000))
    );
    assert_eq!(
        at(now - 5_000_000, now - 5_000_000).status_at(hour, now),
        None
    );
    // Baseline items (added 0) are never new, but can be updated.
    assert_eq!(at(0, 0).status_at(hour, now), None);
    assert_eq!(
        at(0, now - 1000).status_at(hour, now).map(|(s, _)| s),
        Some(ChangeStatus::Updated)
    );
    // No window, or a zero one: never marked.
    assert_eq!(at(now - 1000, now - 1000).status_at(None, now), None);
    assert_eq!(at(now - 1000, now - 1000).status_at(Some(0), now), None);
}

#[tokio::test]
async fn submissions_mark_items_within_the_feeds_window() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db.sqlite3")).await.unwrap();
    let first = json!({
        "title": "Work",
        "description": "Open PRs awaiting my review",
        "new_for": "1h",
        "items": items(&[("a", "A"), ("b", "B")]),
    });
    store.submit("work", &snapshot(first)).await.unwrap();
    // The first submission is the baseline: nothing new, no change reported.
    let feed = store.feed("work").await.unwrap().unwrap();
    assert_eq!(
        feed.info.description.as_deref(),
        Some("Open PRs awaiting my review")
    );
    assert_eq!(feed.info.new_for.as_deref(), Some("1h"));
    assert_eq!(feed.info.last_change, None);
    assert!(feed.changes.values().all(|c| c.status.is_none()));
    assert_eq!(new_count(&store).await, 0);

    // a updated (its color counts as content), b removed, c added.
    let second = json!({"new_for": "1h", "items": [
        {"key": "a", "title": "A", "color": "red"},
        {"key": "c", "title": "C"},
    ]});
    store
        .submit("work", &snapshot(second.clone()))
        .await
        .unwrap();
    let feed = store.feed("work").await.unwrap().unwrap();
    assert_eq!(feed.info.description, None, "replaced by each submission");
    assert_eq!(feed.items[0].color.as_deref(), Some("red"));
    let change = feed.info.last_change.clone().expect("a last change");
    assert_eq!((change.added, change.updated, change.removed), (1, 1, 1));
    assert_eq!(
        change.removed_items,
        [RemovedItem {
            key: "b".into(),
            title: "B".into()
        }]
    );
    assert_eq!(feed.changes["a"].status, Some(ChangeStatus::Updated));
    assert_eq!(feed.changes["c"].status, Some(ChangeStatus::New));
    let summary = &store.feed_summaries().await.unwrap()[0];
    assert_eq!(summary.new_count, 2);
    // The list says when its counts next change: when the marks end.
    let ends = summary.next_wake_at_ms.expect("a deadline");
    assert!(ends > feed.changes["c"].added_at_ms);

    // An identical resubmission changes nothing, including the last change.
    store.submit("work", &snapshot(second)).await.unwrap();
    let again = store.feed("work").await.unwrap().unwrap();
    assert_eq!(again.info.last_change, Some(change));
    assert_eq!(again.changes, feed.changes);

    // Without a window nothing is marked; a short one ends.
    let unmarked =
        json!({"items": [{"key": "a", "title": "A", "color": "red"}, {"key": "c", "title": "C"}]});
    store.submit("work", &snapshot(unmarked)).await.unwrap();
    assert_eq!(status(&store, "c").await, None);
    let short = json!({"new_for": "50ms", "items": [{"key": "d", "title": "D"}]});
    store.submit("work", &snapshot(short)).await.unwrap();
    assert_eq!(status(&store, "d").await, Some(ChangeStatus::New));
    std::thread::sleep(Duration::from_millis(80));
    assert_eq!(status(&store, "d").await, None);
    assert_eq!(new_count(&store).await, 0);
    store.close().await;
}

#[test]
fn submissions_validate_description_window_and_color() {
    let refused = |value: Value| {
        parse_submission(&serde_json::to_vec(&value).unwrap())
            .unwrap_err()
            .to_string()
    };
    let long = "x".repeat(1001);
    assert!(refused(json!({"description": long, "items": []})).contains("description"));
    assert!(refused(json!({"new_for": "soon", "items": []})).contains("new_for"));
    let error = refused(json!({"items": [{"key": "a", "title": "A", "color": "teal"}]}));
    assert!(error.contains("item 0: color must be"), "{error}");
    for color in [
        "red", "orange", "yellow", "green", "blue", "purple", "pink", "gray",
    ] {
        snapshot(json!({"items": [{"key": "a", "title": "A", "color": color}]}));
    }
}

#[tokio::test]
async fn items_from_before_tracking_are_never_new() {
    use sqlx::{Connection, sqlite::SqliteConnectOptions};
    let dir = tempfile::tempdir().unwrap();
    let migrations = dir.path().join("migrations");
    std::fs::create_dir(&migrations).unwrap();
    for (name, sql) in [
        (
            "0001_feeds.sql",
            include_str!("../migrations/0001_feeds.sql"),
        ),
        (
            "0002_boards_items.sql",
            include_str!("../migrations/0002_boards_items.sql"),
        ),
        (
            "0003_feed_view_state.sql",
            include_str!("../migrations/0003_feed_view_state.sql"),
        ),
        (
            "0004_note_url.sql",
            include_str!("../migrations/0004_note_url.sql"),
        ),
        (
            "0005_layouts.sql",
            include_str!("../migrations/0005_layouts.sql"),
        ),
        (
            "0006_preferences.sql",
            include_str!("../migrations/0006_preferences.sql"),
        ),
        (
            "0007_canvas_layouts.sql",
            include_str!("../migrations/0007_canvas_layouts.sql"),
        ),
    ] {
        std::fs::write(migrations.join(name), sql).unwrap();
    }
    let path = dir.path().join("before-tracking.sqlite3");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true);
    let mut conn = sqlx::SqliteConnection::connect_with(&options)
        .await
        .unwrap();
    sqlx::migrate::Migrator::new(migrations.as_path())
        .await
        .unwrap()
        .run(&mut conn)
        .await
        .unwrap();
    sqlx::raw_sql(
        r#"INSERT INTO feeds(name, title, last_submitted_at_ms) VALUES('work', 'Work', 1);
           INSERT INTO feed_items(feed, key, position, content_json, content_hash)
           VALUES('work', 'a', 0, '{"key":"a","title":"A"}', zeroblob(32));"#,
    )
    .execute(&mut conn)
    .await
    .unwrap();
    conn.close().await.unwrap();

    let store = Store::open(&path).await.unwrap();
    let feed = store.feed("work").await.unwrap().unwrap();
    assert_eq!(feed.info.description, None);
    assert_eq!(feed.info.new_for, None);
    assert_eq!(feed.info.last_change, None);
    assert_eq!(feed.changes["a"].added_at_ms, 0);
    // Not a first submission any more: a new key is new, and a changed old
    // item is updated.
    let next = json!({"new_for": "1h", "items": [
        {"key": "a", "title": "A2"}, {"key": "b", "title": "B"},
    ]});
    store.submit("work", &snapshot(next)).await.unwrap();
    assert_eq!(status(&store, "a").await, Some(ChangeStatus::Updated));
    assert_eq!(status(&store, "b").await, Some(ChangeStatus::New));
    store.close().await;
}
