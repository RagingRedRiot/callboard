//! Feeds (DESIGN.md §3, §4): snapshots, failures, item view state, promotion.
use crate::api::{Call, MAX_BODY, Reply, Route, SMALL_BODY, error, reply, route, stored};
use callboard_core::{
    feed::Snapshot,
    store::{FeedItemPatch, StoreError},
};
use hyper::StatusCode;
use serde::Deserialize;
use serde_json::json;

pub(crate) const ROUTES: &[Route] = &[
    route!(GET "/feeds" => list),
    route!(GET "/feeds/{feed}" => get),
    route!(PUT "/feeds/{feed}" => submit),
    route!(DELETE "/feeds/{feed}" => delete),
    route!(POST "/feeds/{feed}/error" => report_error),
    route!(PATCH "/feeds/{feed}/items/{key}" => patch_item),
    route!(POST "/feeds/{feed}/items/{key}/promote" => promote),
];

async fn list(call: Call) -> Reply {
    stored(StatusCode::OK, call.store.feed_summaries().await)
}

async fn get(call: Call) -> Reply {
    match call.store.feed(call.feed()).await {
        Ok(Some(feed)) => reply(StatusCode::OK, feed),
        Ok(None) => error(StatusCode::NOT_FOUND, "feed not found"),
        Err(e) => crate::api::storage_error(e),
    }
}

async fn submit(mut call: Call) -> Reply {
    let snapshot: Snapshot = match call.json(MAX_BODY).await {
        Ok(v) => v,
        Err(rejected) => return rejected.into(),
    };
    stored(
        StatusCode::OK,
        call.store.submit(call.feed(), &snapshot).await,
    )
}

async fn delete(call: Call) -> Reply {
    let deleted = call.store.delete_feed(call.feed()).await;
    stored(StatusCode::OK, deleted.map(|v| json!({"deleted":v})))
}

async fn report_error(mut call: Call) -> Reply {
    #[derive(Deserialize)]
    struct Failure {
        message: String,
    }
    let failure: Failure = match call.json(SMALL_BODY).await {
        Ok(v) => v,
        Err(rejected) => return rejected.into(),
    };
    let recorded = call.store.report_error(call.feed(), &failure.message).await;
    stored(StatusCode::OK, recorded.map(|()| json!({"recorded":true})))
}

async fn patch_item(mut call: Call) -> Reply {
    let patch: FeedItemPatch = match call.json(SMALL_BODY).await {
        Ok(v) => v,
        Err(rejected) => return rejected.into(),
    };
    let result = call
        .store
        .patch_feed_item(call.feed(), call.key(), patch)
        .await;
    stored(StatusCode::OK, result)
}

async fn promote(mut call: Call) -> Reply {
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Kind {
        Todo,
        Note,
    }
    #[derive(Deserialize)]
    struct Promote {
        board_id: i64,
        kind: Kind,
    }
    let request: Promote = match call.json(SMALL_BODY).await {
        Ok(v) => v,
        Err(rejected) => return rejected.into(),
    };
    let (store, feed, key) = (&call.store, call.feed(), call.key());
    let result: Result<serde_json::Value, StoreError> = match request.kind {
        Kind::Todo => store
            .promote_todo(feed, key, request.board_id)
            .await
            .map(|v| json!(v)),
        Kind::Note => store
            .promote_note(feed, key, request.board_id)
            .await
            .map(|v| json!(v)),
    };
    stored(StatusCode::CREATED, result)
}
