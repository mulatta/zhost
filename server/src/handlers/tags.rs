use std::collections::HashMap;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::error::server_error;
use crate::http::access::request_library;
use crate::http::headers::{conflict, current_headers, precondition, version_headers};
use crate::{store, AppState};

/// `GET /users/<id>/tags`: distinct tags with item counts.
pub(crate) async fn tags_get(State(state): State<AppState>) -> Response {
    match store::tags(&state.pool, request_library()).await {
        Ok(value) => (current_headers(&state).await, Json(value)).into_response(),
        Err(error) => server_error("tags", error),
    }
}

/// `DELETE /tags?tags=a || b` — remove tags library-wide under the version
/// guard: strip them from every item and record each in the deletion log. Tags
/// are not objects (no key), so they are addressed by name and split on the
/// Zotero `||` separator. The sync client sends `tags`; the public API documents
/// `tag`, so accept either.
pub(crate) async fn tags_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let expected = match precondition(&headers) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let tags: Vec<String> = params
        .get("tags")
        .or_else(|| params.get("tag"))
        .map(|raw| {
            raw.split("||")
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    match store::delete_tags(&state.pool, request_library(), &tags, Some(expected)).await {
        Ok(store::Outcome::Done(version)) => {
            (StatusCode::NO_CONTENT, version_headers(version)).into_response()
        }
        Ok(store::Outcome::Conflict(current)) => conflict(current),
        Err(error) => server_error("tags delete", error),
    }
}
