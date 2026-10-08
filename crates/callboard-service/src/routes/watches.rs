//! Watches (DESIGN.md §4a): reports, failures, and the user's items. Watch
//! names follow the feed name rules, so they use the `{feed}` capture.
use crate::api::{BODY, Call, MAX_BODY, Reply, Route, SMALL_BODY, error, reply, route, stored};
use callboard_core::{
    store::StoreError,
    watch::{Failure, ItemAction, NewItem, Report},
};
use hyper::StatusCode;
use serde::Serialize;
use serde_json::json;

pub(crate) const ROUTES: &[Route] = &[
    route!(GET "/watches" => list),
    route!(GET "/watches/{feed}" => get),
    route!(DELETE "/watches/{feed}" => delete),
    route!(POST "/watches/{feed}/report" => report),
    route!(POST "/watches/{feed}/error" => report_error),
    route!(POST "/watches/{feed}/items" => add_item),
    route!(PATCH "/watches/{feed}/items/{id}" => act_on_item),
    route!(DELETE "/watches/{feed}/items/{id}" => remove_item),
];

/// Read the JSON body, at most `limit` bytes, or return its error reply.
macro_rules! body {
    ($call:expr, $limit:expr) => {
        match $call.json($limit).await {
            Ok(v) => v,
            Err(rejected) => return rejected.into(),
        }
    };
}

/// As `api::stored`, with the watch errors the shared mapping predates.
fn watch_stored<T: Serialize>(status: StatusCode, result: Result<T, StoreError>) -> Reply {
    match result {
        Err(e @ (StoreError::WatchNotFound(_) | StoreError::WatchItemNotFound { .. })) => {
            error(StatusCode::NOT_FOUND, e)
        }
        result => stored(status, result),
    }
}

async fn list(call: Call) -> Reply {
    stored(StatusCode::OK, call.store.watch_summaries().await)
}

async fn get(call: Call) -> Reply {
    match call.store.watch(call.feed()).await {
        Ok(Some(watch)) => reply(StatusCode::OK, watch),
        Ok(None) => error(StatusCode::NOT_FOUND, "watch not found"),
        Err(e) => crate::api::storage_error(e),
    }
}

async fn delete(call: Call) -> Reply {
    let deleted = call.store.delete_watch(call.feed()).await;
    stored(StatusCode::OK, deleted.map(|v| json!({"deleted":v})))
}

async fn report(mut call: Call) -> Reply {
    let report: Report = body!(call, MAX_BODY);
    watch_stored(
        StatusCode::OK,
        call.store.report_watch(call.feed(), &report).await,
    )
}

async fn report_error(mut call: Call) -> Reply {
    let failure: Failure = body!(call, SMALL_BODY);
    let recorded = call.store.report_watch_error(call.feed(), &failure).await;
    watch_stored(StatusCode::OK, recorded.map(|()| json!({"recorded":true})))
}

async fn add_item(mut call: Call) -> Reply {
    let item: NewItem = body!(call, BODY);
    match call.store.add_watch_item(call.feed(), &item).await {
        Ok(added) => {
            let status = if added.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            reply(status, added)
        }
        Err(e) => watch_stored(StatusCode::OK, Err::<(), _>(e)),
    }
}

async fn act_on_item(mut call: Call) -> Reply {
    let action: ItemAction = body!(call, SMALL_BODY);
    let result = call
        .store
        .act_on_watch_item(call.feed(), call.id(), action)
        .await;
    watch_stored(StatusCode::OK, result)
}

async fn remove_item(call: Call) -> Reply {
    let deleted = call.store.remove_watch_item(call.feed(), call.id()).await;
    watch_stored(StatusCode::OK, deleted.map(|v| json!({"deleted":v})))
}
