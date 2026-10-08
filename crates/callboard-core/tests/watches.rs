//! Watches: user-chosen items that a script reports on (DESIGN.md §4a).
use callboard_core::store::{Change, Store, StoreError};
use callboard_core::watch::{
    Clocks, Failure, ItemAction, ItemState, NewItem, Report, ReportSummary, StateAt, Watch,
};
use serde_json::{Value, json};

const HOUR: i64 = 3_600_000;

async fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db.sqlite3")).await.unwrap();
    (dir, store)
}

fn report(value: Value) -> Report {
    serde_json::from_value(value).unwrap()
}

async fn send(store: &Store, value: Value) -> ReportSummary {
    store.report_watch("tickets", &report(value)).await.unwrap()
}

async fn add(store: &Store, url: &str) -> i64 {
    let item = NewItem {
        url: url.into(),
        label: None,
    };
    store
        .add_watch_item("tickets", &item)
        .await
        .unwrap()
        .item
        .id
}

async fn read(store: &Store) -> Watch {
    store.watch("tickets").await.unwrap().unwrap()
}

/// (id, state) in display order.
async fn queue(store: &Store) -> Vec<(i64, ItemState)> {
    read(store)
        .await
        .items
        .iter()
        .map(|item| (item.id, item.state))
        .collect()
}

fn item(id: i64, fingerprint: &str) -> Value {
    json!({"id": id, "title": format!("Item {id}"), "fingerprint": fingerprint})
}

const ACK: ItemAction = ItemAction {
    acknowledge: true,
    keep_waiting: false,
};
const KEEP: ItemAction = ItemAction {
    acknowledge: false,
    keep_waiting: true,
};

#[test]
fn state_follows_the_clocks_and_windows() {
    let clocks = |attention, waiting| Clocks {
        added_at_ms: 1_000,
        attention_since_ms: attention,
        waiting_since_ms: waiting,
    };
    let at = |c: Clocks, w, q, now| c.state_at(w, q, now);
    let day = Some(24 * HOUR);
    let week = Some(7 * 24 * HOUR);
    // New for waiting_after, then Waiting, then quiet after quiet_after.
    assert_eq!(
        at(clocks(None, None), day, week, 2_000),
        StateAt {
            state: ItemState::New,
            since_ms: 1_000,
            waiting_since_ms: None,
            next_change_ms: Some(1_000 + 24 * HOUR),
        }
    );
    let waiting = 1_000 + 24 * HOUR;
    assert_eq!(
        at(clocks(None, None), day, week, waiting),
        StateAt {
            state: ItemState::Waiting,
            since_ms: waiting,
            waiting_since_ms: Some(waiting),
            next_change_ms: Some(waiting + 7 * 24 * HOUR),
        }
    );
    let quiet = at(clocks(None, None), day, week, waiting + 7 * 24 * HOUR);
    assert_eq!(
        (quiet.state, quiet.since_ms),
        (ItemState::Quiet, waiting + 7 * 24 * HOUR)
    );
    assert_eq!(quiet.next_change_ms, None);
    // An acknowledgement or Keep waiting restarts the waiting clock.
    let restarted = at(clocks(None, Some(50 * HOUR)), day, week, 51 * HOUR);
    assert_eq!(
        (restarted.state, restarted.since_ms),
        (ItemState::Waiting, 50 * HOUR)
    );
    // Attention wins over every window.
    let attention = at(clocks(Some(5_000), Some(10)), day, week, 99 * HOUR);
    assert_eq!(
        (attention.state, attention.since_ms),
        (ItemState::Attention, 5_000)
    );
    // never: new until the first change; never quiet.
    assert_eq!(
        at(clocks(None, None), None, week, 99 * HOUR).state,
        ItemState::New
    );
    let never_quiet = at(clocks(None, None), day, None, 999 * HOUR);
    assert_eq!(
        (never_quiet.state, never_quiet.next_change_ms),
        (ItemState::Waiting, None)
    );
}

#[tokio::test]
async fn the_first_report_creates_the_watch_with_default_windows() {
    let (_dir, store) = store().await;
    let missing = NewItem {
        url: "https://example.com/1".into(),
        label: None,
    };
    assert!(matches!(
        store.add_watch_item("tickets", &missing).await,
        Err(StoreError::WatchNotFound(_))
    ));
    let summary = send(&store, json!({"title": "Tickets", "stale_after": "2h"})).await;
    assert!(summary.created);
    let watch = read(&store).await;
    assert_eq!(watch.info.title, "Tickets");
    assert_eq!(watch.info.stale_after.as_deref(), Some("2h"));
    assert_eq!(watch.info.waiting_after, "1d");
    assert_eq!(watch.info.quiet_after, "7d");
    assert!(watch.items.is_empty());
    // Later reports replace the metadata; omitted fields take defaults.
    let summary = send(
        &store,
        json!({"waiting_after": "never", "quiet_after": "3d"}),
    )
    .await;
    assert!(!summary.created);
    let info = read(&store).await.info;
    assert_eq!(
        (
            info.title.as_str(),
            info.stale_after,
            info.waiting_after.as_str(),
            info.quiet_after.as_str()
        ),
        ("tickets", None, "never", "3d")
    );
}

#[tokio::test]
async fn urls_are_unique_and_labels_belong_to_the_user() {
    let (_dir, store) = store().await;
    send(&store, json!({})).await;
    let first = store
        .add_watch_item(
            "tickets",
            &NewItem {
                url: " https://example.com/1 ".into(),
                label: Some("waiting on support".into()),
            },
        )
        .await
        .unwrap();
    assert!(first.created);
    assert_eq!(first.item.url, "https://example.com/1");
    assert_eq!(first.item.state, ItemState::New);
    let again = store
        .add_watch_item(
            "tickets",
            &NewItem {
                url: "https://example.com/1".into(),
                label: Some("other".into()),
            },
        )
        .await
        .unwrap();
    assert!(!again.created);
    assert_eq!(again.item.id, first.item.id);
    assert_eq!(again.item.label.as_deref(), Some("waiting on support"));
    assert_eq!(read(&store).await.items.len(), 1);
    let empty = NewItem {
        url: "  ".into(),
        label: None,
    };
    assert!(matches!(
        store.add_watch_item("tickets", &empty).await,
        Err(StoreError::Validation(_))
    ));
}

#[tokio::test]
async fn only_a_changed_fingerprint_raises_attention() {
    let (_dir, store) = store().await;
    send(&store, json!({})).await;
    let id = add(&store, "https://example.com/1").await;
    // The first report is the baseline.
    let summary = send(&store, json!({"items": [item(id, "1")]})).await;
    assert!(summary.attention_ids.is_empty());
    assert_eq!(queue(&store).await, [(id, ItemState::New)]);
    // Other fields change without counting as news.
    let summary = send(
        &store,
        json!({"items": [{"id": id, "title": "Renamed", "fingerprint": "1", "meta": {"status": "open"}}]}),
    )
    .await;
    assert!(summary.attention_ids.is_empty());
    let watch = read(&store).await;
    assert_eq!(watch.items[0].content.as_ref().unwrap().title, "Renamed");
    assert_eq!(watch.items[0].changed_at_ms, None);
    // A changed fingerprint does, even while the item is new.
    let summary = send(&store, json!({"items": [item(id, "2")]})).await;
    assert_eq!(summary.attention_ids, [id]);
    let watch = read(&store).await;
    assert_eq!(watch.items[0].state, ItemState::Attention);
    assert!(watch.items[0].changed_at_ms.is_some());
    // Already needing attention: a further change lists nothing new.
    let summary = send(&store, json!({"items": [item(id, "3")]})).await;
    assert!(summary.attention_ids.is_empty());
}

#[tokio::test]
async fn acknowledging_moves_an_item_to_waiting_until_the_next_change() {
    let (_dir, store) = store().await;
    send(&store, json!({})).await;
    let id = add(&store, "https://example.com/1").await;
    send(&store, json!({"items": [item(id, "1")]})).await;
    // Acknowledging an item that needs no attention does nothing.
    let unchanged = store.act_on_watch_item("tickets", id, ACK).await.unwrap();
    assert_eq!(unchanged.state, ItemState::New);
    send(&store, json!({"items": [item(id, "2")]})).await;
    let acked = store.act_on_watch_item("tickets", id, ACK).await.unwrap();
    assert_eq!(acked.state, ItemState::Waiting);
    assert_eq!(acked.waiting_since_ms, Some(acked.state_since_ms));
    // It does not return to New.
    assert_eq!(queue(&store).await, [(id, ItemState::Waiting)]);
    send(&store, json!({"items": [item(id, "3")]})).await;
    assert_eq!(queue(&store).await, [(id, ItemState::Attention)]);
    assert!(matches!(
        store
            .act_on_watch_item("tickets", id, ItemAction::default())
            .await,
        Err(StoreError::InvalidPatch(_))
    ));
    assert!(matches!(
        store.act_on_watch_item("tickets", 999, ACK).await,
        Err(StoreError::WatchItemNotFound { .. })
    ));
}

#[tokio::test]
async fn a_script_acknowledges_after_comparing_the_fingerprint() {
    let (_dir, store) = store().await;
    send(&store, json!({})).await;
    let id = add(&store, "https://example.com/1").await;
    send(&store, json!({"items": [item(id, "1")]})).await;
    // Changed and acknowledged in one report: recorded, never needs attention.
    let mut changed = item(id, "2");
    changed["acknowledge"] = json!(true);
    let summary = send(&store, json!({"items": [changed]})).await;
    assert!(summary.attention_ids.is_empty());
    let watch = read(&store).await;
    assert_eq!(watch.items[0].state, ItemState::Waiting);
    assert_eq!(watch.items[0].fingerprint.as_deref(), Some("2"));
    assert!(watch.items[0].changed_at_ms.is_some());
    // An acknowledgement sent on every run does nothing when unneeded.
    let mut same = item(id, "2");
    same["acknowledge"] = json!(true);
    let before = read(&store).await.items[0].state_since_ms;
    send(&store, json!({"items": [same]})).await;
    assert_eq!(read(&store).await.items[0].state_since_ms, before);
}

#[tokio::test]
async fn the_queue_puts_the_longest_waiting_first_in_each_section() {
    let (_dir, store) = store().await;
    // Every report carries the watch's windows, as a script's would.
    let send = |items: Value| {
        let store = &store;
        async move {
            let windows = json!({"waiting_after": "never", "quiet_after": "never", "items": items});
            store
                .report_watch("tickets", &report(windows))
                .await
                .unwrap()
        }
    };
    send(json!([])).await;
    let a = add(&store, "https://example.com/a").await;
    let b = add(&store, "https://example.com/b").await;
    let c = add(&store, "https://example.com/c").await;
    let d = add(&store, "https://example.com/d").await;
    send(json!([
        item(a, "1"),
        item(b, "1"),
        item(c, "1"),
        item(d, "1")
    ]))
    .await;
    assert_eq!(
        queue(&store).await,
        [
            (a, ItemState::New),
            (b, ItemState::New),
            (c, ItemState::New),
            (d, ItemState::New)
        ]
    );
    // c changes before a; both need attention, c first.
    send(json!([item(c, "2")])).await;
    send(json!([item(a, "2")])).await;
    // A further change keeps c's place.
    send(json!([item(c, "3")])).await;
    // b is acknowledged into Waiting via a change; d stays New.
    send(json!([item(b, "2")])).await;
    store.act_on_watch_item("tickets", b, ACK).await.unwrap();
    assert_eq!(
        queue(&store).await,
        [
            (c, ItemState::Attention),
            (a, ItemState::Attention),
            (d, ItemState::New),
            (b, ItemState::Waiting),
        ]
    );
    let summary = &store.watch_summaries().await.unwrap()[0];
    assert_eq!(
        (
            summary.item_count,
            summary.attention_count,
            summary.new_count,
            summary.quiet_count
        ),
        (4, 2, 1, 0)
    );
    assert_eq!(summary.next_wake_at_ms, None);
}

#[tokio::test]
async fn quiet_items_are_listed_once_and_keep_waiting_restarts_the_clock() {
    let (_dir, store) = store().await;
    // Zero windows: items are quiet as soon as they are added.
    let send = |quiet_after: &'static str, items: Value| {
        let store = &store;
        async move {
            let report_body =
                json!({"waiting_after": "0s", "quiet_after": quiet_after, "items": items});
            store
                .report_watch("tickets", &report(report_body))
                .await
                .unwrap()
        }
    };
    send("0s", json!([])).await;
    let a = add(&store, "https://example.com/a").await;
    let summary = send("0s", json!([item(a, "1")])).await;
    assert_eq!(summary.quiet_ids, [a]);
    assert_eq!(queue(&store).await, [(a, ItemState::Quiet)]);
    let summary = send("0s", json!([])).await;
    assert!(summary.quiet_ids.is_empty());
    let kept = store.act_on_watch_item("tickets", a, KEEP).await.unwrap();
    assert!(kept.waiting_since_ms.is_some());
    // Quiet again at once with a zero window, so listed again.
    let summary = send("0s", json!([])).await;
    assert_eq!(summary.quiet_ids, [a]);
    // With a longer window, the item is Waiting and Keep waiting does nothing.
    send("1h", json!([])).await;
    let waiting = store.act_on_watch_item("tickets", a, KEEP).await.unwrap();
    assert_eq!(waiting.state, ItemState::Waiting);
    assert_eq!(waiting.waiting_since_ms, kept.waiting_since_ms);
    let watch = read(&store).await;
    assert_eq!(
        watch.next_wake_at_ms,
        Some(kept.waiting_since_ms.unwrap() + HOUR)
    );
}

#[tokio::test]
async fn unknown_ids_are_ignored_and_errors_keep_the_last_good_report() {
    let (_dir, store) = store().await;
    send(&store, json!({})).await;
    let id = add(&store, "https://example.com/1").await;
    store
        .report_watch("other", &report(json!({})))
        .await
        .unwrap();
    let foreign = store
        .add_watch_item(
            "other",
            &NewItem {
                url: "https://example.com/x".into(),
                label: None,
            },
        )
        .await
        .unwrap()
        .item
        .id;
    let summary = send(
        &store,
        json!({"items": [item(id, "1"), item(foreign, "1"), item(999, "1")]}),
    )
    .await;
    assert_eq!(summary.reported, 1);
    assert_eq!(summary.ignored_ids, [foreign, 999]);
    let summary = send(
        &store,
        json!({"items": [{"id": id, "error": "could not read the page"}]}),
    )
    .await;
    assert_eq!(summary.reported, 1);
    let watch = read(&store).await;
    assert_eq!(
        watch.items[0].error.as_ref().unwrap().message,
        "could not read the page"
    );
    assert_eq!(watch.items[0].fingerprint.as_deref(), Some("1"));
    send(&store, json!({"items": [item(id, "1")]})).await;
    assert_eq!(read(&store).await.items[0].error, None);
    // Whole-watch and per-item failures through the error route.
    let failure = |id, message: &str| Failure {
        id,
        message: message.into(),
    };
    store
        .report_watch_error("tickets", &failure(None, "rate limited"))
        .await
        .unwrap();
    store
        .report_watch_error("tickets", &failure(Some(id), "gone"))
        .await
        .unwrap();
    let watch = read(&store).await;
    assert_eq!(watch.info.error.unwrap().message, "rate limited");
    assert_eq!(watch.items[0].error.as_ref().unwrap().message, "gone");
    assert!(matches!(
        store
            .report_watch_error("missing", &failure(None, "x"))
            .await,
        Err(StoreError::WatchNotFound(_))
    ));
    assert!(matches!(
        store
            .report_watch_error("tickets", &failure(Some(foreign), "x"))
            .await,
        Err(StoreError::WatchItemNotFound { .. })
    ));
    // The next report clears the watch error.
    send(&store, json!({})).await;
    assert_eq!(read(&store).await.info.error, None);
}

#[tokio::test]
async fn reports_are_validated_before_anything_is_written() {
    let (_dir, store) = store().await;
    for (bad, reason) in [
        (
            json!({"items": [{"id": 1, "title": "T"}]}),
            "fingerprint is required",
        ),
        (
            json!({"items": [{"id": 1, "fingerprint": "1"}]}),
            "title is required",
        ),
        (
            json!({"items": [{"id": 1, "error": "x", "title": "T"}]}),
            "carries only id and error",
        ),
        (
            json!({"items": [item(1, "1"), item(1, "2")]}),
            "duplicate id",
        ),
        (
            json!({"items": [{"id": 1, "title": "T", "fingerprint": "x".repeat(257)}]}),
            "fingerprint exceeds",
        ),
        (json!({"quiet_after": "soon"}), "invalid quiet_after"),
        (
            json!({"items": [{"id": 1, "title": "T", "fingerprint": "1", "color": "teal"}]}),
            "color",
        ),
    ] {
        let error = store
            .report_watch("tickets", &report(bad))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(reason), "{error}");
    }
    assert!(serde_json::from_value::<Report>(json!({"items": [{"id": 1, "url": "x"}]})).is_err());
    assert!(
        store
            .report_watch("Bad Name", &Report::default())
            .await
            .is_err()
    );
    assert!(store.watch_summaries().await.unwrap().is_empty());
}

#[tokio::test]
async fn deleting_removes_items_and_ids_are_never_reused() {
    let (_dir, store) = store().await;
    send(&store, json!({})).await;
    let first = add(&store, "https://example.com/1").await;
    assert!(store.remove_watch_item("tickets", first).await.unwrap());
    assert!(!store.remove_watch_item("tickets", first).await.unwrap());
    let second = add(&store, "https://example.com/1").await;
    assert!(second > first);
    // A report for the removed ID does not reach the new item.
    let summary = send(&store, json!({"items": [item(first, "1")]})).await;
    assert_eq!(summary.ignored_ids, [first]);
    assert!(store.delete_watch("tickets").await.unwrap());
    assert!(!store.delete_watch("tickets").await.unwrap());
    assert!(store.watch("tickets").await.unwrap().is_none());
    send(&store, json!({})).await;
    assert!(read(&store).await.items.is_empty());
}

#[tokio::test]
async fn writes_notify_and_no_ops_stay_silent() {
    let (_dir, store) = store().await;
    let mut changes = store.subscribe();
    let notice = Change::Watch {
        name: "tickets".into(),
    };
    send(&store, json!({})).await;
    assert_eq!(changes.try_recv().unwrap(), notice);
    let id = add(&store, "https://example.com/1").await;
    assert_eq!(changes.try_recv().unwrap(), notice);
    add(&store, "https://example.com/1").await;
    store.act_on_watch_item("tickets", id, ACK).await.unwrap();
    store.act_on_watch_item("tickets", id, KEEP).await.unwrap();
    assert!(changes.try_recv().is_err());
    store.remove_watch_item("tickets", id).await.unwrap();
    assert_eq!(changes.try_recv().unwrap(), notice);
}
