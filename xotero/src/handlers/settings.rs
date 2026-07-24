use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::domain::LibraryAccess;
use crate::error::server_error;
use crate::http::access::{request_access, request_library};
use crate::http::headers::{
    conflict, csv_of, current_headers, if_modified_since, precondition, since_check, since_of,
    version_headers,
};
use crate::{store, AppState};

pub(crate) async fn settings_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // The client may send the cursor as ?since= or the If-Modified-Since-Version
    // header; honour whichever is higher.
    let since = since_of(&params).max(if_modified_since(&headers));
    let (current, fresh) = since_check(&state, since).await;
    if fresh {
        return (StatusCode::NOT_MODIFIED, version_headers(current)).into_response();
    }
    match store::settings(&state.pool, request_library()).await {
        Ok(value) => (version_headers(current), Json(value)).into_response(),
        Err(error) => server_error("settings", error),
    }
}

fn group_admin_setting_denied<'a>(
    access: LibraryAccess,
    keys: impl Iterator<Item = &'a str>,
) -> bool {
    const ADMIN_ONLY: [&str; 3] = [
        "attachmentRenameTemplate",
        "autoRenameFiles",
        "autoRenameFilesFileTypes",
    ];
    access.group_id.is_some()
        && !access.is_admin
        && keys.into_iter().any(|key| ADMIN_ONLY.contains(&key))
}

pub(crate) async fn settings_write(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let expected = match precondition(&headers) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let value: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    if group_admin_setting_denied(
        request_access(),
        value
            .as_object()
            .into_iter()
            .flat_map(|settings| settings.keys().map(String::as_str)),
    ) {
        return (StatusCode::FORBIDDEN, "group admin setting denied").into_response();
    }
    match store::write_settings(&state.pool, request_library(), value, Some(expected)).await {
        Ok(store::Outcome::Done(version)) => {
            (StatusCode::NO_CONTENT, version_headers(version)).into_response()
        }
        Ok(store::Outcome::Conflict(current)) => conflict(current),
        Err(error) => server_error("settings write", error),
    }
}

/// `DELETE /settings?settingKey=k1,k2` — remove the named settings under the
/// version guard and record them in the deletion log. (Reusing settings_write
/// here was a no-op: a DELETE has no body, so it deleted nothing yet returned
/// 204 and the setting persisted.)
pub(crate) async fn settings_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let expected = match precondition(&headers) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let keys = csv_of(&params, "settingKey");
    if group_admin_setting_denied(request_access(), keys.iter().map(String::as_str)) {
        return (StatusCode::FORBIDDEN, "group admin setting denied").into_response();
    }
    match store::delete_settings(&state.pool, request_library(), &keys, Some(expected)).await {
        Ok(store::Outcome::Done(version)) => {
            (StatusCode::NO_CONTENT, version_headers(version)).into_response()
        }
        Ok(store::Outcome::Conflict(current)) => conflict(current),
        Err(error) => server_error("settings delete", error),
    }
}

pub(crate) async fn deleted(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let since = since_of(&params);
    match store::deleted(&state.pool, request_library(), since).await {
        Ok(value) => (current_headers(&state).await, Json(value)).into_response(),
        Err(error) => server_error("deleted", error),
    }
}
