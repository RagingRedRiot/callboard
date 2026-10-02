//! Preferences: the startup layout (DESIGN.md §6.4).
use crate::api::{BODY, Call, Reply, Route, route, stored};
use callboard_core::store::PreferencesPatch;
use hyper::StatusCode;

pub(crate) const ROUTES: &[Route] = &[
    route!(GET "/preferences" => get),
    route!(PATCH "/preferences" => patch),
];

async fn get(call: Call) -> Reply {
    stored(StatusCode::OK, call.store.preferences().await)
}

async fn patch(mut call: Call) -> Reply {
    let patch: PreferencesPatch = match call.json(BODY).await {
        Ok(v) => v,
        Err(rejected) => return rejected.into(),
    };
    stored(StatusCode::OK, call.store.patch_preferences(patch).await)
}
