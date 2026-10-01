//! Saved layouts (DESIGN.md §6.4).
use crate::api::{BODY, Call, Reply, Route, route, stored};
use callboard_core::layout::{Layout, LayoutRename};
use hyper::StatusCode;
use serde_json::json;

pub(crate) const ROUTES: &[Route] = &[
    route!(GET "/layouts" => list),
    route!(PUT "/layouts/{name}" => save),
    route!(PATCH "/layouts/{name}" => rename),
    route!(DELETE "/layouts/{name}" => delete),
];

async fn list(call: Call) -> Reply {
    stored(StatusCode::OK, call.store.list_layouts().await)
}

async fn save(mut call: Call) -> Reply {
    let layout: Layout = match call.json(BODY).await {
        Ok(v) => v,
        Err(rejected) => return rejected.into(),
    };
    stored(
        StatusCode::OK,
        call.store.save_layout(call.name(), &layout).await,
    )
}

async fn rename(mut call: Call) -> Reply {
    let rename: LayoutRename = match call.json(BODY).await {
        Ok(v) => v,
        Err(rejected) => return rejected.into(),
    };
    let layout = call.store.rename_layout(call.name(), &rename.name).await;
    stored(StatusCode::OK, layout)
}

async fn delete(call: Call) -> Reply {
    let deleted = call.store.delete_layout(call.name()).await;
    stored(StatusCode::OK, deleted.map(|v| json!({"deleted":v})))
}
