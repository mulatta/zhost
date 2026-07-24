//! Self-hosted Zotero Web API v3 sync server.
//!
//! Objects are stored as opaque jsonb blobs in PostgreSQL (see `store`); each
//! write bumps a single library version counter so the client's `since` reads
//! and `If-Unmodified-Since-Version` writes stay coherent. See SPEC.md for the
//! protocol contract.

mod config;
mod domain;
mod error;
mod handlers;
mod http;
mod query;
mod s3;
mod store;

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Form, OriginalUri, Path, Query, RawQuery, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::config::Config;
use crate::domain::{LibraryAccess, LibraryId, Permissions, RequestContext};
use crate::error::{s3_error, server_error};
use crate::handlers::groups::{group_get, groups};
use crate::handlers::keys::key_current;
use crate::handlers::login::{
    cancel_session, check_session, create_session, login_authorize, login_page,
};
use crate::http::access::{request_access, request_library, request_permissions};
use crate::http::headers::{
    conflict, csv_of, current_headers, header_value, if_modified_since, next_link, precondition,
    since_check, since_of, version_headers,
};
use crate::http::middleware::{log_and_auth, MAX_BODY};
use crate::http::validation::{valid_key, valid_object_key};

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    pool: PgPool,
    storage: Arc<s3::Storage>,
}

fn upload_storage_key(library_id: LibraryId, upload_token: &str) -> String {
    format!("libraries/{}/uploads/{upload_token}", library_id.get())
}

fn key_hash(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

fn recovery_key_permissions(config: &Config, hash: &[u8]) -> Option<Permissions> {
    config
        .keys
        .iter()
        .find_map(|(key, permissions)| (key_hash(key) == hash).then_some(*permissions))
}

/// An unguessable upload token (128 bits of OS randomness, hex-encoded). `None`
/// if the OS RNG can't be read, so the caller can fail the request rather than
/// panic.
pub(crate) fn upload_token() -> Option<String> {
    use std::io::Read;
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .ok()?;
    Some(buf.iter().map(|b| format!("{b:02x}")).collect())
}

// --- library data -----------------------------------------------------------

/// `format=versions&since=N` returns the changed `{key: version}` map; otherwise
/// `?<kind>Key=a,b&format=json` returns the full `[{key, version, data}]`.
async fn read(state: &AppState, kind: &str, params: HashMap<String, String>) -> Response {
    if params.get("format").map(String::as_str) == Some("versions") {
        let since = since_of(&params);
        // Always 200 with the (possibly empty) versions map. The client's
        // `getVersions` sends no `If-Modified-Since-Version` header and treats a
        // 304 as "no data", which then mismatches its library-version check and
        // makes it restart the sync forever. 304 is only for the header path
        // (settings), not for `?since=` versions reads.
        let current = store::current_version(&state.pool, request_library())
            .await
            .unwrap_or(0);
        return match store::versions(&state.pool, request_library(), kind, since, true).await {
            Ok(value) => (version_headers(current), Json(value)).into_response(),
            Err(error) => server_error("read", error),
        };
    }
    let keys = csv_of(&params, &format!("{kind}Key"));
    match store::objects(&state.pool, request_library(), kind, &keys, true).await {
        Ok(value) => (current_headers(state).await, Json(value)).into_response(),
        Err(error) => server_error("read", error),
    }
}

/// The two sync reads shared by `/items` and `/items/top`: the `format=versions`
/// map and the `?itemKey=…` batch. With `top`, the versions map is restricted to
/// top-level items (the client's parent-first phase). Returns `None` when the
/// request carries neither, i.e. it is a CLI query rather than a sync read.
async fn item_sync_read(state: &AppState, params: &query::Params, top: bool) -> Option<Response> {
    if params.get("format") == Some("versions") {
        let since = params
            .get("since")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        // Always 200 + map (never 304); see `read` — a 304 here loops the client.
        let current = store::current_version(&state.pool, request_library())
            .await
            .unwrap_or(0);
        let result = if top {
            store::top_versions(
                &state.pool,
                request_library(),
                since,
                request_permissions().notes,
            )
            .await
        } else {
            store::versions(
                &state.pool,
                request_library(),
                "item",
                since,
                request_permissions().notes,
            )
            .await
        };
        return Some(match result {
            Ok(value) => (version_headers(current), Json(value)).into_response(),
            Err(error) => server_error("items versions", error),
        });
    }
    if let Some(csv) = params.get("itemKey") {
        let keys: Vec<String> = csv.split(',').map(String::from).collect();
        return Some(
            match store::objects(
                &state.pool,
                request_library(),
                "item",
                &keys,
                request_permissions().notes,
            )
            .await
            {
                Ok(value) => (current_headers(state).await, Json(value)).into_response(),
                Err(error) => server_error("items batch", error),
            },
        );
    }
    None
}

/// Render an item query as a paged JSON listing: the `[{key, version, data}]`
/// array plus `Total-Results` and, while more rows remain, a `Link: …;
/// rel="next"` built against `path` (the public-URL endpoint).
async fn item_listing(
    state: &AppState,
    path: &str,
    raw: Option<&str>,
    q: &query::ItemQuery,
) -> Response {
    match store::query_items(
        &state.pool,
        request_library(),
        q,
        request_permissions().notes,
    )
    .await
    {
        Ok((items, total)) => {
            let mut headers = current_headers(state).await;
            headers.insert("total-results", total.to_string().parse().unwrap());
            if q.start + q.limit < total {
                let link = next_link(&state.config, path, raw, q.start + q.limit);
                headers.insert("link", link.parse().unwrap());
            }
            (headers, Json(Value::Array(items))).into_response()
        }
        Err(error) => server_error("items query", error),
    }
}

/// `format=keys` returns every matching item key (no paging) as a plain-text
/// newline list — the shape Zotero's `getKeys()` parses (it reads the body as
/// `responseText.split('\n')`). Returns `None` for any other format.
async fn item_keys_response(
    state: &AppState,
    params: &query::Params,
    q: &query::ItemQuery,
) -> Option<Response> {
    if params.get("format") != Some("keys") {
        return None;
    }
    Some(
        match store::item_keys(
            &state.pool,
            request_library(),
            q,
            request_permissions().notes,
        )
        .await
        {
            // A `String` body sets `Content-Type: text/plain`, which is what the
            // client expects; current_headers adds `Last-Modified-Version`.
            Ok(keys) => (current_headers(state).await, keys.join("\n")).into_response(),
            Err(error) => server_error("item keys", error),
        },
    )
}

/// `GET /users/<id>/items`: the two sync reads, or the CLI query when neither.
async fn items_get(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = query::Params::parse(raw.as_deref());
    if let Some(resp) = item_sync_read(&state, &params, false).await {
        return resp;
    }
    let q = query::ItemQuery::from_params(&params);
    if let Some(resp) = item_keys_response(&state, &params, &q).await {
        return resp;
    }
    item_listing(&state, uri.path(), raw.as_deref(), &q).await
}

/// `GET /users/<id>/items/top`: top-level items (no `parentItem`). Also answers
/// the sync `format=versions` (top-filtered) and `itemKey` reads sent here.
async fn items_top(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = query::Params::parse(raw.as_deref());
    if let Some(resp) = item_sync_read(&state, &params, true).await {
        return resp;
    }
    let mut q = query::ItemQuery::from_params(&params);
    q.top = true;
    if let Some(resp) = item_keys_response(&state, &params, &q).await {
        return resp;
    }
    item_listing(&state, uri.path(), raw.as_deref(), &q).await
}

/// `GET /users/<id>/items/trash`: only trashed items (`data.deleted`).
async fn items_trash(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = query::Params::parse(raw.as_deref());
    let mut q = query::ItemQuery::from_params(&params);
    q.only_trashed = true;
    if let Some(resp) = item_keys_response(&state, &params, &q).await {
        return resp;
    }
    item_listing(&state, uri.path(), raw.as_deref(), &q).await
}

/// `GET /users/<id>/collections/<key>/items`: items in the given collection.
async fn collection_items(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path((_id, key)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = query::Params::parse(raw.as_deref());
    let mut q = query::ItemQuery::from_params(&params);
    q.collection = Some(key.clone());
    if let Some(resp) = item_keys_response(&state, &params, &q).await {
        return resp;
    }
    item_listing(&state, uri.path(), raw.as_deref(), &q).await
}

/// `GET /users/<id>/collections/<key>/items/top`: top-level items in the
/// collection. The sync client requests this with `format=keys` when restoring a
/// previously-deleted collection (syncEngine.js `_restoreRestoredCollectionItems`).
async fn collection_items_top(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Path((_id, key)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = query::Params::parse(raw.as_deref());
    let mut q = query::ItemQuery::from_params(&params);
    q.collection = Some(key.clone());
    q.top = true;
    if let Some(resp) = item_keys_response(&state, &params, &q).await {
        return resp;
    }
    item_listing(&state, uri.path(), raw.as_deref(), &q).await
}

/// `GET /users/<id>/tags`: distinct tags with item counts.
async fn tags_get(State(state): State<AppState>) -> Response {
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
async fn tags_delete(
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

/// Both POST and PATCH create-or-update with merge semantics (see `store::write`):
/// the Zotero client uploads only an existing object's changed fields, so omitted
/// fields must be preserved.
async fn write(state: &AppState, kind: &str, headers: HeaderMap, body: Bytes) -> Response {
    let batch: Vec<Value> = match serde_json::from_slice(&body) {
        Ok(batch) => batch,
        Err(error) => {
            tracing::warn!(%error, "malformed write body");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    // Reject a malformed object key up front (a keyless object is fine — the
    // store assigns one). A bad key would otherwise sync and the client would
    // refuse it with "key is not valid".
    if let Some(bad) = batch
        .iter()
        .filter_map(|o| o.get("key").and_then(Value::as_str))
        .find(|k| !valid_object_key(k))
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("invalid object key: {bad}"),
        )
            .into_response();
    }
    let expected = match precondition(&headers) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let access = request_access();
    let allow_stored_file_write = access.group_id.is_none() || access.file_write;
    match store::write(
        &state.pool,
        access.library_id,
        kind,
        batch,
        Some(expected),
        allow_stored_file_write,
    )
    .await
    {
        Ok(store::ObjectMutation::Done((version, successful))) => (
            version_headers(version),
            Json(json!({
                "successful": successful,
                "success": {},
                "unchanged": {},
                "failed": {},
            })),
        )
            .into_response(),
        Ok(store::ObjectMutation::Conflict(current)) => conflict(current),
        Ok(store::ObjectMutation::FileWriteDenied) => {
            (StatusCode::FORBIDDEN, "group file editing denied").into_response()
        }
        Err(error) => server_error("write", error),
    }
}

async fn delete(
    state: &AppState,
    kind: &str,
    headers: HeaderMap,
    params: HashMap<String, String>,
) -> Response {
    let expected = match precondition(&headers) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let keys = csv_of(&params, &format!("{kind}Key"));
    let access = request_access();
    let allow_stored_file_write = access.group_id.is_none() || access.file_write;
    match store::delete(
        &state.pool,
        access.library_id,
        kind,
        &keys,
        Some(expected),
        allow_stored_file_write,
    )
    .await
    {
        Ok(store::ObjectMutation::Done(version)) => {
            (StatusCode::NO_CONTENT, version_headers(version)).into_response()
        }
        Ok(store::ObjectMutation::Conflict(current)) => conflict(current),
        Ok(store::ObjectMutation::FileWriteDenied) => {
            (StatusCode::FORBIDDEN, "group file editing denied").into_response()
        }
        Err(error) => server_error("delete", error),
    }
}

async fn settings_read(
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

async fn settings_write(
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
async fn settings_delete(
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

async fn deleted(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let since = since_of(&params);
    match store::deleted(&state.pool, request_library(), since).await {
        Ok(value) => (current_headers(&state).await, Json(value)).into_response(),
        Err(error) => server_error("deleted", error),
    }
}

/// `GET /fulltext?format=versions&since=N` → `{itemKey: version}` for content
/// changed after `since`, so the client downloads only what it lacks.
async fn fulltext_versions(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let since = since_of(&params);
    // Always 200 + map (never 304); see `read` — a 304 here loops the client.
    let current = store::current_version(&state.pool, request_library())
        .await
        .unwrap_or(0);
    match store::fulltext_versions(&state.pool, request_library(), since).await {
        Ok(value) => (version_headers(current), Json(value)).into_response(),
        Err(error) => server_error("fulltext versions", error),
    }
}

/// `GET /items/<key>/fulltext` → the item's content object, with the row's
/// version in `Last-Modified-Version` (the client stores it to skip re-fetching).
async fn fulltext_item(
    State(state): State<AppState>,
    Path((_id, key)): Path<(String, String)>,
) -> Response {
    match store::fulltext_item(&state.pool, request_library(), &key).await {
        Ok(Some((version, data))) => (version_headers(version), Json(data)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => server_error("fulltext item", error),
    }
}

/// `POST /fulltext` — store a batch of extracted content, returning the per-index
/// result map the client reads to mark each item synced (or `412` if stale).
async fn fulltext_write(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let batch: Vec<Value> = match serde_json::from_slice(&body) {
        Ok(batch) => batch,
        Err(error) => {
            tracing::warn!(%error, "malformed fulltext body");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let expected = match precondition(&headers) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match store::write_fulltext(&state.pool, request_library(), batch, Some(expected)).await {
        Ok(store::Outcome::Done((version, successful))) => (
            version_headers(version),
            Json(json!({
                "successful": successful,
                "success": {},
                "unchanged": {},
                "failed": {},
            })),
        )
            .into_response(),
        Ok(store::Outcome::Conflict(current)) => conflict(current),
        Err(error) => server_error("fulltext write", error),
    }
}

/// Attachment file endpoint. The same path serves both POST steps:
/// authorisation (`md5`/`filename`/`filesize`/`mtime` form) and registration
/// (`upload` form, after the bytes have been PUT to the upload URL).
async fn file_post(
    State(state): State<AppState>,
    Extension(context): Extension<RequestContext>,
    Path((_id, key)): Path<(String, String)>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if !valid_key(&key) {
        return (StatusCode::BAD_REQUEST, "invalid item key").into_response();
    }
    // Registration step: the client posts upload=<token> after PUTting the bytes
    // to the upload endpoint, which verified them and stored the object.
    if let Some(token) = form.get("upload") {
        let token_hash = key_hash(token);
        let upload = match store::pending_upload(&state.pool, &token_hash).await {
            Ok(Some(upload)) => upload,
            Ok(None) => {
                return (StatusCode::BAD_REQUEST, "no pending upload").into_response();
            }
            Err(error) => return server_error("pending upload lookup", error),
        };
        if upload.item_key != key {
            return (StatusCode::BAD_REQUEST, "upload token does not match item").into_response();
        }
        if upload.library_id != request_library() {
            return (StatusCode::FORBIDDEN, "upload token library mismatch").into_response();
        }
        if upload.group_id != request_access().group_id {
            return (StatusCode::FORBIDDEN, "upload token group mismatch").into_response();
        }
        if upload.state != store::PendingUploadState::Uploaded {
            return (StatusCode::BAD_REQUEST, "no uploaded bytes").into_response();
        }
        let outcome = store::register_file(&state.pool, &token_hash, request_library(), &key).await;
        return match outcome {
            Ok(store::FileRegistrationOutcome::Done(version)) => {
                (StatusCode::NO_CONTENT, version_headers(version)).into_response()
            }
            Ok(store::FileRegistrationOutcome::Conflict(current)) => {
                cleanup_pending_uploads(&state).await;
                conflict(current)
            }
            Ok(store::FileRegistrationOutcome::InvalidItem) => {
                cleanup_pending_uploads(&state).await;
                (StatusCode::CONFLICT, "attachment item changed").into_response()
            }
            Ok(store::FileRegistrationOutcome::InvalidUpload) => {
                (StatusCode::BAD_REQUEST, "no pending upload").into_response()
            }
            Err(error) => server_error("register file", error),
        };
    }

    // Authorization step. The client sends a precondition: `If-None-Match: *`
    // for a new file, or `If-Match: <oldmd5>` to replace an existing one. Without
    // either, the version guard would be bypassed (428, as zfs.js expects).
    let if_none_match = headers.contains_key("if-none-match");
    let if_match = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    if !if_none_match && if_match.is_none() {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            "If-Match or If-None-Match required",
        )
            .into_response();
    }
    match store::stored_file_attachment_exists(&state.pool, request_library(), &key).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return server_error("file attachment lookup", error),
    }

    let md5 = form.get("md5").cloned().unwrap_or_default();
    let stored_md5 = match store::file_meta(&state.pool, request_library(), &key).await {
        Ok(meta) => meta.map(|meta| meta.md5),
        Err(error) => return server_error("file auth", error),
    };
    let zip_md5 = form.get("zipMD5").filter(|value| !value.is_empty());
    let zip_filename = form.get("zipFilename").filter(|value| !value.is_empty());
    if zip_md5.is_some() != zip_filename.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            "zipMD5 and zipFilename must be provided together",
        )
            .into_response();
    }
    let compressed = zip_md5.is_some()
        || form
            .get("zip")
            .is_some_and(|value| !value.is_empty() && value != "0");
    let upload_md5 = zip_md5.cloned().unwrap_or_else(|| md5.clone());

    // md5 hex compares case-insensitively, matching the verification in
    // upload_put (which lowercases) so dedup and replace agree on normalization.
    if if_none_match {
        // "Only if no file exists." Same md5 → already uploaded (dedup); a
        // different existing file → conflict.
        if let Some(existing) = &stored_md5 {
            if existing.eq_ignore_ascii_case(&md5) {
                return (current_headers(&state).await, Json(json!({ "exists": 1 })))
                    .into_response();
            }
            return conflict(
                store::current_version(&state.pool, request_library())
                    .await
                    .unwrap_or(0),
            );
        }
    } else if let Some(want) = &if_match {
        // "Only if the current md5 matches." Otherwise → conflict.
        if !stored_md5
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case(want))
        {
            return conflict(
                store::current_version(&state.pool, request_library())
                    .await
                    .unwrap_or(0),
            );
        }
    }

    // Authorize: persist only the capability digest. Candidate bytes use a
    // separate random object ID so the database never stores the raw token.
    cleanup_pending_uploads(&state).await;
    let Some(token) = upload_token() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Some(candidate_id) = upload_token() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let upload = store::PendingUpload {
        library_id: request_library(),
        group_id: request_access().group_id,
        authorizer_key_hash: key_hash(&context.presented_key),
        bootstrap_user_id: state
            .config
            .keys
            .contains_key(&context.presented_key)
            .then_some(context.principal.user_id),
        item_key: key.clone(),
        expected_md5: if if_none_match {
            None
        } else {
            stored_md5.clone()
        },
        blob_key: upload_storage_key(request_library(), &candidate_id),
        md5,
        upload_md5,
        filename: form.get("filename").cloned().unwrap_or_default(),
        filesize: form
            .get("filesize")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        mtime: form.get("mtime").and_then(|s| s.parse().ok()).unwrap_or(0),
        compressed,
        state: store::PendingUploadState::Authorized,
    };
    if let Err(error) = store::create_pending_upload(&state.pool, &key_hash(&token), &upload).await
    {
        return server_error("create pending upload", error);
    }
    // Empty prefix/suffix: the client PUTs the raw file bytes to url.
    Json(json!({
        "url": format!("{}/uploads/{}", state.config.public_url, token),
        "uploadKey": token,
        "contentType": if compressed { "application/zip" } else { "application/octet-stream" },
        "prefix": "",
        "suffix": "",
    }))
    .into_response()
}

async fn upload_scope_authorized(
    state: &AppState,
    upload: &store::PendingUpload,
    principal: &crate::domain::Principal,
    api_key_id: Option<crate::domain::ApiKeyId>,
    permissions: Permissions,
    static_permissions: Option<Permissions>,
) -> sqlx::Result<bool> {
    let Some(group_id) = upload.group_id else {
        return Ok(principal.library_id == upload.library_id
            && permissions.library
            && permissions.write
            && permissions.files);
    };
    match store::resolve_group_library(
        &state.pool,
        group_id,
        principal.user_id,
        api_key_id,
        static_permissions,
    )
    .await?
    {
        store::GroupLibraryResolution::Allowed(access) => {
            Ok(access.library_id == upload.library_id && access.file_write)
        }
        store::GroupLibraryResolution::Denied | store::GroupLibraryResolution::Missing => Ok(false),
    }
}

async fn cleanup_pending_uploads(state: &AppState) {
    loop {
        let garbage = match store::claim_pending_upload_garbage(&state.pool, 32).await {
            Ok(garbage) => garbage,
            Err(error) => {
                tracing::warn!(%error, "claim pending upload garbage");
                return;
            }
        };
        if garbage.is_empty() {
            return;
        }
        for candidate in garbage {
            if let Err(error) = state.storage.delete(&candidate.blob_key).await {
                tracing::warn!(%error, "delete pending upload candidate");
                continue;
            }
            if let Err(error) =
                store::delete_pending_upload_garbage(&state.pool, &candidate.token_hash).await
            {
                tracing::warn!(%error, "finish pending upload cleanup");
            }
        }
    }
}

async fn pending_upload_cleanup_loop(state: AppState) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        cleanup_pending_uploads(&state).await;
    }
}

/// Receive the raw attachment bytes for a pending upload token, verify them
/// against the authorized md5/filesize, and store an immutable candidate.
/// Rejects an unknown token. Verifying here (where the bytes are in hand) keeps
/// the integrity check server-side now that the bytes go straight to S3.
async fn upload_put(
    State(state): State<AppState>,
    Path(token): Path<String>,
    body: Bytes,
) -> Response {
    let token_hash = key_hash(&token);
    let upload = match store::pending_upload(&state.pool, &token_hash).await {
        Ok(Some(upload)) => upload,
        Ok(None) => {
            return (StatusCode::BAD_REQUEST, "unknown upload token").into_response();
        }
        Err(error) => return server_error("pending upload lookup", error),
    };
    if upload.state != store::PendingUploadState::Authorized {
        return (StatusCode::CONFLICT, "upload token already used").into_response();
    }
    if upload.bootstrap_user_id.is_none() {
        let authenticated =
            match store::authenticate_api_key(&state.pool, &upload.authorizer_key_hash).await {
                Ok(authenticated) => authenticated,
                Err(error) => return server_error("upload authorization", error),
            };
        let Some((api_key_id, principal, permissions)) = authenticated else {
            return (StatusCode::FORBIDDEN, "API key revoked").into_response();
        };
        match upload_scope_authorized(
            &state,
            &upload,
            &principal,
            Some(api_key_id),
            permissions,
            None,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => return (StatusCode::FORBIDDEN, "upload access revoked").into_response(),
            Err(error) => return server_error("upload authorization", error),
        }
    } else if let Some(user_id) = upload.bootstrap_user_id {
        if user_id != state.config.user_id {
            return (StatusCode::FORBIDDEN, "bootstrap user changed").into_response();
        }
        let principal = match store::bootstrap_principal(&state.pool, state.config.user_id).await {
            Ok(Some(principal)) => principal,
            Ok(None) => return (StatusCode::FORBIDDEN, "bootstrap user disabled").into_response(),
            Err(error) => return server_error("upload authorization", error),
        };
        let Some(permissions) =
            recovery_key_permissions(&state.config, &upload.authorizer_key_hash)
        else {
            return (StatusCode::FORBIDDEN, "recovery key removed").into_response();
        };
        match upload_scope_authorized(
            &state,
            &upload,
            &principal,
            None,
            permissions,
            Some(permissions),
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => return (StatusCode::FORBIDDEN, "upload access revoked").into_response(),
            Err(error) => return server_error("upload authorization", error),
        }
    }
    let actual_md5 = {
        use md5::{Digest, Md5};
        format!("{:x}", Md5::new().chain_update(&body).finalize())
    };
    if body.len() as i64 != upload.filesize || actual_md5 != upload.upload_md5.to_lowercase() {
        tracing::warn!("uploaded bytes do not match authorization");
        return (
            StatusCode::BAD_REQUEST,
            "uploaded bytes do not match md5/filesize",
        )
            .into_response();
    }
    match store::claim_pending_upload(&state.pool, &token_hash).await {
        Ok(true) => {}
        Ok(false) => {
            return (StatusCode::CONFLICT, "upload token already used").into_response();
        }
        Err(error) => return server_error("claim pending upload", error),
    }
    let content_type = if upload.compressed {
        "application/zip"
    } else {
        "application/octet-stream"
    };
    if let Err(error) = state
        .storage
        .put(&upload.blob_key, &body, content_type)
        .await
    {
        if let Err(reset_error) = store::reset_pending_upload(&state.pool, &token_hash).await {
            tracing::warn!(error = %reset_error, "reset failed pending upload");
        }
        cleanup_pending_uploads(&state).await;
        return s3_error("store file", error);
    }
    match store::mark_pending_upload_uploaded(&state.pool, &token_hash).await {
        Ok(true) => {}
        Ok(false) => {
            if let Err(error) = store::discard_pending_upload(&state.pool, &token_hash).await {
                tracing::warn!(%error, "discard incomplete pending upload");
            }
            cleanup_pending_uploads(&state).await;
            return (StatusCode::CONFLICT, "upload token already used").into_response();
        }
        Err(error) => return server_error("complete pending upload", error),
    }
    StatusCode::CREATED.into_response()
}

/// The client reads md5/mtime from this response's headers and then downloads
/// the bytes from `Location` — a short-lived pre-signed GET URL pointing straight
/// at the bucket, so the read path bypasses this server entirely (and the URL is
/// an unguessable, expiring capability the client follows without an API key).
async fn file_get(
    State(state): State<AppState>,
    Path((_id, key)): Path<(String, String)>,
) -> Response {
    if !valid_key(&key) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let meta = match store::file_meta(&state.pool, request_library(), &key).await {
        Ok(Some(meta)) => meta,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return server_error("file meta", error),
    };
    let url = match state.storage.presign_get(&meta.blob_key).await {
        Ok(url) => url,
        Err(error) => return s3_error("presign download", error),
    };
    let mut headers = HeaderMap::new();
    headers.insert("location", header_value(&url));
    headers.insert(
        "zotero-file-modification-time",
        header_value(&meta.mtime.to_string()),
    );
    headers.insert("zotero-file-md5", header_value(&meta.blob_md5));
    headers.insert("zotero-file-size", header_value(&meta.filesize.to_string()));
    headers.insert(
        "zotero-file-compressed",
        header_value(if meta.compressed { "Yes" } else { "No" }),
    );
    (StatusCode::FOUND, headers).into_response()
}

fn app(state: AppState) -> Router {
    // Each object kind shares the read/write/delete logic; the closures bind the
    // kind so the handlers stay generic.
    let objects = |kind: &'static str| {
        get(
            move |State(state): State<AppState>,
                  Query(p): Query<HashMap<String, String>>| async move {
                read(&state, kind, p).await
            },
        )
        .post(
            move |State(state): State<AppState>, headers: HeaderMap, body: Bytes| async move {
                write(&state, kind, headers, body).await
            },
        )
        .patch(
            move |State(state): State<AppState>, headers: HeaderMap, body: Bytes| async move {
                write(&state, kind, headers, body).await
            },
        )
            .delete(
                move |State(state): State<AppState>,
                      headers: HeaderMap,
                      Query(p): Query<HashMap<String, String>>| async move {
                    delete(&state, kind, headers, p).await
                },
            )
    };
    Router::new()
        .route("/keys/current", get(key_current))
        .route("/keys/sessions", post(create_session))
        .route(
            "/keys/sessions/{token}",
            get(check_session).delete(cancel_session),
        )
        .route("/login", get(login_page).post(login_authorize))
        .route("/users/{id}/groups", get(groups))
        .route("/groups/{id}", get(group_get).head(group_get))
        .route(
            "/groups/{id}/settings",
            get(settings_read)
                .post(settings_write)
                .delete(settings_delete),
        )
        .route("/groups/{id}/collections", objects("collection"))
        .route(
            "/groups/{id}/collections/{key}/items",
            get(collection_items),
        )
        .route(
            "/groups/{id}/collections/{key}/items/top",
            get(collection_items_top),
        )
        .route("/groups/{id}/searches", objects("search"))
        .route(
            "/groups/{id}/items",
            get(items_get)
                .post(
                    move |State(state): State<AppState>,
                          headers: HeaderMap,
                          body: Bytes| async move {
                        write(&state, "item", headers, body).await
                    },
                )
                .patch(
                    move |State(state): State<AppState>,
                          headers: HeaderMap,
                          body: Bytes| async move {
                        write(&state, "item", headers, body).await
                    },
                )
                .delete(
                    move |State(state): State<AppState>,
                          headers: HeaderMap,
                          Query(params): Query<HashMap<String, String>>| async move {
                        delete(&state, "item", headers, params).await
                    },
                ),
        )
        .route("/groups/{id}/items/top", get(items_top))
        .route("/groups/{id}/items/trash", get(items_trash))
        .route(
            "/groups/{id}/tags",
            get(tags_get).delete(tags_delete),
        )
        .route(
            "/groups/{id}/fulltext",
            get(fulltext_versions).post(fulltext_write),
        )
        .route(
            "/groups/{id}/items/{key}/fulltext",
            get(fulltext_item),
        )
        .route(
            "/groups/{id}/items/{key}/file",
            get(file_get).post(file_post),
        )
        .route("/groups/{id}/deleted", get(deleted))
        .route(
            "/users/{id}/settings",
            get(settings_read)
                .post(settings_write)
                .delete(settings_delete),
        )
        .route("/users/{id}/collections", objects("collection"))
        // CLI listing of a collection's items, plus the top-level variant the
        // sync client fetches with format=keys when restoring a collection.
        .route("/users/{id}/collections/{key}/items", get(collection_items))
        .route(
            "/users/{id}/collections/{key}/items/top",
            get(collection_items_top),
        )
        .route("/users/{id}/searches", objects("search"))
        // Items share the write/delete logic but take a dedicated GET that adds
        // the CLI query API alongside the two sync reads.
        .route(
            "/users/{id}/items",
            get(items_get)
                .post(
                    move |State(state): State<AppState>,
                          headers: HeaderMap,
                          body: Bytes| async move {
                        write(&state, "item", headers, body).await
                    },
                )
                .patch(
                    move |State(state): State<AppState>,
                          headers: HeaderMap,
                          body: Bytes| async move {
                        write(&state, "item", headers, body).await
                    },
                )
                .delete(
                    move |State(state): State<AppState>,
                          headers: HeaderMap,
                          Query(p): Query<HashMap<String, String>>| async move {
                        delete(&state, "item", headers, p).await
                    },
                ),
        )
        .route("/users/{id}/items/top", get(items_top))
        .route("/users/{id}/items/trash", get(items_trash))
        .route("/users/{id}/tags", get(tags_get).delete(tags_delete))
        .route(
            "/users/{id}/fulltext",
            get(fulltext_versions).post(fulltext_write),
        )
        .route("/users/{id}/items/{key}/fulltext", get(fulltext_item))
        .route(
            "/users/{id}/items/{key}/file",
            get(file_get).post(file_post),
        )
        .route("/uploads/{key}", post(upload_put))
        .route("/users/{id}/deleted", get(deleted))
        // Attachment uploads exceed the default 2 MiB extractor limit; raise it
        // to MAX_BODY (the middleware enforces the same bound while buffering).
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            log_and_auth,
        ))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Arc::new(Config::from_env());

    let pool = store::connect(&config.database_url)
        .await
        .expect("connect to database");
    store::bootstrap_identity(
        &pool,
        config.user_id,
        &config.username,
        &config.display_name,
        config.library_id,
    )
    .await
    .expect("bootstrap identity");
    let bootstrap_oidc_issuer = std::env::var("ZHOST_BOOTSTRAP_OIDC_ISSUER").ok();
    let bootstrap_oidc_subject = std::env::var("ZHOST_BOOTSTRAP_OIDC_SUBJECT").ok();
    match (bootstrap_oidc_issuer, bootstrap_oidc_subject) {
        (Some(issuer), Some(subject)) if !issuer.is_empty() && !subject.is_empty() => {
            store::bootstrap_external_identity(&pool, &issuer, &subject, config.user_id)
                .await
                .expect("bootstrap external identity");
        }
        (None, None) => {}
        _ => panic!("bootstrap OIDC issuer and subject must both be nonempty"),
    }
    let storage = Arc::new(s3::Storage::new(&config.s3).expect("init object storage"));
    let state = AppState {
        config,
        pool,
        storage,
    };
    store::reset_interrupted_pending_uploads(&state.pool)
        .await
        .expect("recover interrupted uploads");
    cleanup_pending_uploads(&state).await;
    tokio::spawn(pending_upload_cleanup_loop(state.clone()));

    let listener = tokio::net::TcpListener::bind(&state.config.bind)
        .await
        .expect("bind address");
    tracing::info!(bind = %state.config.bind, "zhost listening");
    axum::serve(listener, app(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server run");
}

#[cfg(test)]
mod middleware_tests {
    use super::{is_group_discovery_path, path_group_data_id};

    #[test]
    fn group_permission_bypass_is_limited_to_discovery_routes() {
        assert!(is_group_discovery_path("/groups/303"));
        assert!(is_group_discovery_path("/users/101/groups"));
        assert!(!is_group_discovery_path("/groups/303/items"));
        assert!(!is_group_discovery_path("/admin/users/101/groups"));
    }

    #[test]
    fn group_data_paths_resolve_only_nested_positive_ids() {
        assert_eq!(
            path_group_data_id("/groups/303/items")
                .unwrap()
                .map(|id| id.get()),
            Some(303)
        );
        assert_eq!(path_group_data_id("/groups/303").unwrap(), None);
        assert_eq!(path_group_data_id("/users/303/items").unwrap(), None);
        assert!(path_group_data_id("/groups/not-an-id/items").is_err());
        assert!(path_group_data_id("/groups/0/items").is_err());
    }
}
