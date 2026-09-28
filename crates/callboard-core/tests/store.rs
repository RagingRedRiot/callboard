use callboard_core::feed::{ChangeSummary, MAX_SNAPSHOT_BYTES, Snapshot, parse_submission};
use callboard_core::store::{Store, StoreError};
use serde_json::{Value, json};
use sqlx::Connection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use tempfile::TempDir;

fn snapshot(value: Value) -> Snapshot {
    parse_submission(&serde_json::to_vec(&value).unwrap()).unwrap()
}

async fn setup() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("callboard.sqlite3"))
        .await
        .unwrap();
    (dir, store)
}

async fn inspect(dir: &TempDir) -> SqliteConnection {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(dir.path().join("callboard.sqlite3"))
            .foreign_keys(true),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn migrations_wal_and_data_survive_reopening() {
    let (dir, store) = setup().await;
    let submission = snapshot(json!({
        "title":"Reviews", "source_url":"https://example.com", "stale_after":"1h",
        "items":[{"key":"z", "title":"First", "meta":{"ready":true}}, {"key":"a", "title":"Second"}]
    }));
    let changes = store.submit("reviews", &submission).await.unwrap();
    assert_eq!(changes.added_keys, ["z", "a"]);
    let saved = store.feed("reviews").await.unwrap().unwrap();
    assert_eq!(saved.items, submission.items);
    assert_eq!(saved.info.title, "Reviews");
    assert_eq!(saved.info.source_url, submission.source_url);
    assert_eq!(saved.info.stale_after, submission.stale_after);
    assert!(saved.info.last_submitted_at_ms > 0);
    store.close().await;

    let reopened = Store::open(dir.path().join("callboard.sqlite3"))
        .await
        .unwrap();
    assert_eq!(reopened.feed("reviews").await.unwrap(), Some(saved.clone()));
    assert_eq!(reopened.list_feeds().await.unwrap(), [saved.info]);
    let mut conn = inspect(&dir).await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
            .fetch_one(&mut conn)
            .await
            .unwrap(),
        "wal"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(&mut conn)
            .await
            .unwrap(),
        1
    );
    conn.close().await.unwrap();
    reopened.close().await;
}

#[tokio::test]
async fn summaries_and_order_follow_complete_snapshots() {
    let (_dir, store) = setup().await;
    store
        .submit(
            "work",
            &snapshot(json!([
                {"key":"gone-z", "title":"Gone"}, {"key":"keep", "title":"Keep"},
                {"key":"gone-a", "title":"Gone"}, {"key":"update", "title":"Before"}
            ])),
        )
        .await
        .unwrap();
    let next = snapshot(json!([
        {"key":"new-z", "title":"New"}, {"key":"update", "title":"After"},
        {"key":"keep", "title":"Keep"}, {"key":"new-a", "title":"New"}
    ]));
    let changes = store.submit("work", &next).await.unwrap();
    assert_eq!(
        changes,
        ChangeSummary {
            added: 2,
            removed: 2,
            updated: 1,
            unchanged: 1,
            added_keys: vec!["new-z".into(), "new-a".into()],
            removed_keys: vec!["gone-z".into(), "gone-a".into()],
            updated_keys: vec!["update".into()],
        }
    );
    assert_eq!(store.feed("work").await.unwrap().unwrap().items, next.items);
    assert_eq!(
        store.submit("work", &next).await.unwrap(),
        ChangeSummary {
            unchanged: 4,
            ..Default::default()
        }
    );
    store.close().await;
}

#[tokio::test]
async fn normalization_and_reordering_do_not_mark_content_updated() {
    let (dir, store) = setup().await;
    let first = parse_submission(
        br#"[
        {"key":"b", "title":"B", "meta":{"z":true, "a":2}},
        {"key":"a", "title":"A"}
    ]"#,
    )
    .unwrap();
    store.submit("work", &first).await.unwrap();
    let mut conn = inspect(&dir).await;
    let before =
        sqlx::query_as::<_, (String, i64)>("SELECT key, rowid FROM feed_items ORDER BY key")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    let reordered = parse_submission(
        br#"[
        {"title":"A", "key":"a", "url":null, "body":null, "tags":[], "meta":{}},
        {"title":"B", "key":"b", "meta":{"a":2, "z":true}}
    ]"#,
    )
    .unwrap();
    assert_eq!(
        store.submit("work", &reordered).await.unwrap(),
        ChangeSummary {
            unchanged: 2,
            ..Default::default()
        }
    );
    assert_eq!(
        store.feed("work").await.unwrap().unwrap().items,
        reordered.items
    );
    let after =
        sqlx::query_as::<_, (String, i64)>("SELECT key, rowid FROM feed_items ORDER BY key")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    assert_eq!(before, after, "retained keys must keep their rows");
    conn.close().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn all_content_fields_participate_in_updates() {
    let (_dir, store) = setup().await;
    let mut value = json!({"key":"a", "title":"A"});
    store
        .submit("work", &snapshot(json!([value.clone()])))
        .await
        .unwrap();
    for (field, replacement) in [
        ("title", json!("Changed")),
        ("body", json!("Details")),
        ("url", json!("https://example.com")),
        ("tags", json!(["review"])),
        ("meta", json!({"ready":true})),
    ] {
        value[field] = replacement;
        let changes = store
            .submit("work", &snapshot(json!([value.clone()])))
            .await
            .unwrap();
        assert_eq!(
            changes,
            ChangeSummary {
                updated: 1,
                updated_keys: vec!["a".into()],
                ..Default::default()
            },
            "{field}"
        );
    }
    store.close().await;
}

#[tokio::test]
async fn failures_preserve_items_and_identical_submissions_refresh_status() {
    let (dir, store) = setup().await;
    let submission = snapshot(json!([{"key":"a", "title":"A"}]));
    store.submit("work", &submission).await.unwrap();
    // Set a known older timestamp, avoiding sleeps and clock-resolution races.
    let mut conn = inspect(&dir).await;
    sqlx::query("UPDATE feeds SET last_submitted_at_ms = 1 WHERE name = 'work'")
        .execute(&mut conn)
        .await
        .unwrap();
    store.report_error("work", "upstream failed").await.unwrap();
    let failed = store.feed("work").await.unwrap().unwrap();
    assert_eq!(failed.items, submission.items);
    assert_eq!(failed.info.last_submitted_at_ms, 1);
    let error = failed.info.error.unwrap();
    assert_eq!(error.message, "upstream failed");
    assert!(error.at_ms > 1);
    store.report_error("work", "still failing").await.unwrap();
    assert_eq!(
        store
            .feed("work")
            .await
            .unwrap()
            .unwrap()
            .info
            .error
            .unwrap()
            .message,
        "still failing"
    );
    assert_eq!(
        store.submit("work", &submission).await.unwrap().unchanged,
        1
    );
    let refreshed = store.feed("work").await.unwrap().unwrap();
    assert!(refreshed.info.error.is_none());
    assert!(refreshed.info.last_submitted_at_ms > 1);
    assert_eq!(refreshed.items, submission.items);
    conn.close().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn metadata_replaces_and_defaults_without_content_updates() {
    let (_dir, store) = setup().await;
    store.submit("work", &snapshot(json!({"title":"Title", "source_url":"https://example.com", "stale_after":"1h", "items":[]}))).await.unwrap();
    assert_eq!(
        store.submit("work", &snapshot(json!([]))).await.unwrap(),
        ChangeSummary::default()
    );
    let info = store.feed("work").await.unwrap().unwrap().info;
    assert_eq!(info.title, "work");
    assert_eq!(info.source_url, None);
    assert_eq!(info.stale_after, None);
    store.close().await;
}

#[tokio::test]
async fn clearing_deleting_and_reappearing_keys_are_new() {
    let (dir, store) = setup().await;
    let one = snapshot(json!([{"key":"a", "title":"A"}]));
    for name in ["work", "other"] {
        store.submit(name, &one).await.unwrap();
    }
    let empty = store.submit("work", &snapshot(json!([]))).await.unwrap();
    assert_eq!(empty.removed_keys, ["a"]);
    assert_eq!(empty.removed, 1);
    assert!(store.feed("work").await.unwrap().unwrap().items.is_empty());
    assert_eq!(store.submit("work", &one).await.unwrap().added, 1);
    assert!(store.delete_feed("work").await.unwrap());
    assert!(!store.delete_feed("work").await.unwrap());
    assert!(store.feed("work").await.unwrap().is_none());
    let mut conn = inspect(&dir).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM feed_items WHERE feed = 'work'")
            .fetch_one(&mut conn)
            .await
            .unwrap(),
        0
    );
    assert_eq!(store.feed("other").await.unwrap().unwrap().items, one.items);
    assert_eq!(store.submit("work", &one).await.unwrap().added_keys, ["a"]);
    assert_eq!(
        store
            .list_feeds()
            .await
            .unwrap()
            .iter()
            .map(|info| info.name.as_str())
            .collect::<Vec<_>>(),
        ["other", "work"]
    );
    conn.close().await.unwrap();
    store.close().await;
}

#[tokio::test]
async fn invalid_submissions_leave_existing_state_untouched() {
    let (_dir, store) = setup().await;
    let valid = snapshot(json!([{"key":"a", "title":"A"}]));
    store.submit("work", &valid).await.unwrap();
    store.report_error("work", "fetch failed").await.unwrap();
    let before = store.feed("work").await.unwrap();
    let mut duplicate = valid.clone();
    duplicate.items.push(duplicate.items[0].clone());
    let mut too_large = valid.clone();
    too_large.items[0].url = Some("x".repeat(MAX_SNAPSHOT_BYTES));
    let mut bad_title = valid.clone();
    bad_title.items[0].title = "x".repeat(501);
    for invalid in [duplicate, too_large, bad_title] {
        assert!(store.submit("work", &invalid).await.is_err());
        assert!(store.submit("new", &invalid).await.is_err());
        assert_eq!(store.feed("work").await.unwrap(), before);
        assert!(store.feed("new").await.unwrap().is_none());
    }
    assert!(matches!(
        store.report_error("unknown", "failed").await,
        Err(StoreError::FeedNotFound(_))
    ));
    assert!(store.feed("unknown").await.unwrap().is_none());
    assert!(store.submit("BAD", &valid).await.is_err());
    assert!(store.feed("BAD").await.is_err());
    assert!(store.report_error("BAD", "failed").await.is_err());
    assert!(store.delete_feed("BAD").await.is_err());
    store.close().await;
}

#[tokio::test]
async fn database_failure_rolls_back_partial_item_and_metadata_writes() {
    let (dir, store) = setup().await;
    store
        .submit(
            "work",
            &snapshot(json!({"title":"Original", "items":[{"key":"old", "title":"Old"}]})),
        )
        .await
        .unwrap();
    store
        .report_error("work", "preserve this error")
        .await
        .unwrap();
    let before = store.feed("work").await.unwrap();
    let mut conn = inspect(&dir).await;
    sqlx::query("CREATE TRIGGER fail_item BEFORE INSERT ON feed_items WHEN NEW.key = 'fail' BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .execute(&mut conn).await.unwrap();
    let next = snapshot(json!({"title":"Changed", "items":[
        {"key":"first", "title":"Inserted before failure"}, {"key":"fail", "title":"Abort"}
    ]}));
    assert!(matches!(
        store.submit("work", &next).await,
        Err(StoreError::Database(_))
    ));
    assert_eq!(store.feed("work").await.unwrap(), before);
    assert!(store.submit("new", &next).await.is_err());
    assert!(store.feed("new").await.unwrap().is_none());
    sqlx::query("DROP TRIGGER fail_item")
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(store.submit("work", &next).await.unwrap().added, 2);
    assert_eq!(store.feed("work").await.unwrap().unwrap().items, next.items);
    conn.close().await.unwrap();
    store.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_writers_compare_against_last_committed_snapshot() {
    let (dir, first) = setup().await;
    let second = Store::open(dir.path().join("callboard.sqlite3"))
        .await
        .unwrap();
    let submission = snapshot(json!([{"key":"shared", "title":"Shared"}]));
    let (a, b) = tokio::join!(
        first.submit("work", &submission),
        second.submit("work", &submission)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.added + b.added, 1);
    assert_eq!(a.unchanged + b.unchanged, 1);
    let left = snapshot(json!([{"key":"left", "title":"Left"}]));
    let right = snapshot(json!([{"key":"right", "title":"Right"}]));
    let (a, b) = tokio::join!(first.submit("work", &left), second.submit("work", &right));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!((a.added, a.removed, b.added, b.removed), (1, 1, 1, 1));
    let final_items = first.feed("work").await.unwrap().unwrap().items;
    if a.removed_keys == ["shared"] {
        assert_eq!(b.removed_keys, ["left"]);
        assert_eq!(final_items, right.items);
    } else {
        assert_eq!(b.removed_keys, ["shared"]);
        assert_eq!(a.removed_keys, ["right"]);
        assert_eq!(final_items, left.items);
    }
    first.close().await;
    second.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readers_never_mix_metadata_and_items_from_different_snapshots() {
    let (_dir, store) = setup().await;
    let make = |n| {
        snapshot(json!({"title":format!("v{n}"), "items":[{"key":"a", "title":format!("v{n}")}]}))
    };
    store.submit("work", &make(0)).await.unwrap();
    let writer = async {
        for n in 1..=30 {
            store.submit("work", &make(n)).await.unwrap();
        }
    };
    let reader = async {
        for _ in 0..60 {
            let feed = store.feed("work").await.unwrap().unwrap();
            assert_eq!(feed.items.len(), 1);
            assert_eq!(feed.info.title, feed.items[0].title);
        }
    };
    tokio::join!(writer, reader);
    store.close().await;
}
