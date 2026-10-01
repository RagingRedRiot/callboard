//! Feed descriptions and change tracking (DESIGN.md §3.1, §4.3).
use callboard_core::feed::{Snapshot, parse_submission};
use callboard_core::store::{Change, ChangeStatus, RemovedItem, Store, StoreError};
use serde_json::{Value, json};

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

async fn unseen(store: &Store) -> usize {
    store.feed_summaries().await.unwrap()[0].unseen_count
}

#[tokio::test]
async fn submissions_record_new_updated_and_removed_items_until_seen() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db.sqlite3")).await.unwrap();
    let first = json!({
        "title": "Work",
        "description": "Open PRs awaiting my review",
        "items": items(&[("a", "A"), ("b", "B")]),
    });
    store.submit("work", &snapshot(first)).await.unwrap();
    // The first submission is the baseline: seen, and no change to report.
    let feed = store.feed("work").await.unwrap().unwrap();
    assert_eq!(
        feed.info.description.as_deref(),
        Some("Open PRs awaiting my review")
    );
    assert_eq!(feed.info.last_change, None);
    assert!(feed.changes.values().all(|c| c.status.is_none()));
    assert!(feed.changes["a"].added_at_ms > 0);
    assert_eq!(unseen(&store).await, 0);

    // a updated, b removed, c added; the description is dropped.
    let second = json!({"items": [
        {"key": "a", "title": "A", "body": "new body"},
        {"key": "c", "title": "C"},
    ]});
    store
        .submit("work", &snapshot(second.clone()))
        .await
        .unwrap();
    let feed = store.feed("work").await.unwrap().unwrap();
    assert_eq!(feed.info.description, None);
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
    assert!(feed.changes["a"].changed_at_ms >= feed.changes["a"].added_at_ms);
    assert_eq!(feed.changes["c"].status, Some(ChangeStatus::New));
    assert_eq!(unseen(&store).await, 2);

    // An identical resubmission changes nothing, including the last change.
    store.submit("work", &snapshot(second)).await.unwrap();
    let again = store.feed("work").await.unwrap().unwrap();
    assert_eq!(again.info.last_change, Some(change));
    assert_eq!(again.changes, feed.changes);

    // Seeing one item, then all of them; each emits a feed notice.
    let mut notices = store.subscribe();
    assert_eq!(store.mark_seen("work", Some("c")).await.unwrap(), 1);
    assert!(matches!(notices.try_recv(), Ok(Change::Feed { name }) if name == "work"));
    assert_eq!(status(&store, "c").await, None);
    assert_eq!(status(&store, "a").await, Some(ChangeStatus::Updated));
    assert_eq!(store.mark_seen("work", None).await.unwrap(), 2);
    assert_eq!(unseen(&store).await, 0);

    // A later change marks it updated again.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let third = json!({"items": [
        {"key": "a", "title": "A", "body": "newer body"},
        {"key": "c", "title": "C"},
    ]});
    store.submit("work", &snapshot(third)).await.unwrap();
    assert_eq!(status(&store, "a").await, Some(ChangeStatus::Updated));
    assert_eq!(status(&store, "c").await, None);

    assert!(matches!(
        store.mark_seen("work", Some("missing")).await,
        Err(StoreError::FeedItemNotFound { .. })
    ));
    assert!(matches!(
        store.mark_seen("nope", None).await,
        Err(StoreError::FeedNotFound(_))
    ));
    store.close().await;
}

#[tokio::test]
async fn descriptions_are_limited_to_1000_characters() {
    let long = "x".repeat(1001);
    let input = serde_json::to_vec(&json!({"description": long, "items": []})).unwrap();
    let error = parse_submission(&input).unwrap_err();
    assert!(error.to_string().contains("description"), "{error}");
}

#[tokio::test]
async fn items_from_before_tracking_count_as_seen() {
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
    assert_eq!(feed.info.last_change, None);
    let change = feed.changes["a"];
    assert_eq!((change.added_at_ms, change.status), (0, None));
    // Not the first submission any more: a new key is new.
    let next = json!({"items": items(&[("a", "A"), ("b", "B")])});
    store.submit("work", &snapshot(next)).await.unwrap();
    assert_eq!(status(&store, "b").await, Some(ChangeStatus::New));
    store.close().await;
}
