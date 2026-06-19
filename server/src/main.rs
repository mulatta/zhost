//! Self-hosted Zotero Web API v3 sync server.
//!
//! Objects are stored as opaque jsonb blobs in PostgreSQL (see `store`); each
//! write bumps a single library version counter so the client's `since` reads
//! and `If-Unmodified-Since-Version` writes stay coherent. See SPEC.md for the
//! protocol contract.

mod query;
mod s3;
mod store;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};

use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Form, Path, Query, RawQuery, Request},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use sqlx::PgPool;

/// What a key may do. Reads are always allowed; `write` gates mutations.
#[derive(Clone, Copy)]
struct Access {
    write: bool,
}

struct Config {
    /// Bearer token → access. Provisioned out of band, loaded from secret files
    /// at boot; never minted by the server or stored in the database.
    keys: HashMap<String, Access>,
    /// A read/write token handed to the app through the login session.
    login_key: String,
    user_id: u64,
    bind: String,
    /// Client-facing base URL (e.g. the reverse-proxy address). Used for the
    /// login and upload URLs handed to the client, which must be reachable by it
    /// — not the internal bind address. (Downloads use a pre-signed bucket URL.)
    public_url: String,
    database_url: String,
    s3: s3::Config,
    /// If set, `POST /login` requires the front proxy to forward a matching
    /// authenticated identity (`X-Auth-Request-Email`/`-User`). Unset on a
    /// private network, where reachability is the gate.
    login_authorized_user: Option<String>,
}

static CFG: OnceLock<Config> = OnceLock::new();
static POOL: OnceLock<PgPool> = OnceLock::new();
static STORAGE: OnceLock<s3::Storage> = OnceLock::new();

/// In-flight file uploads, keyed by an unguessable upload token (not the item
/// key, which is guessable) and remembered between the authorisation, upload and
/// registration steps. Pruned on insert so a never-completed upload can't leak.
static PENDING: LazyLock<Mutex<HashMap<String, PendingUpload>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// How long an authorized-but-unfinished upload stays valid.
const PENDING_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// In-flight login sessions, keyed by an unguessable session token. A session is
/// created `authorized: false`; the `/login` step (gated by the front proxy's
/// SSO in production) flips it true, and only then does polling hand out the key.
/// Pruned on insert so an abandoned session can't linger.
static SESSIONS: LazyLock<Mutex<HashMap<String, LoginSession>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// How long a login session stays valid; enrollment is a one-off, prompt action.
const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(600);

struct LoginSession {
    authorized: bool,
    created: std::time::Instant,
}

#[derive(Clone)]
struct PendingUpload {
    /// The attachment item the bytes belong to (and the object key in the bucket).
    item_key: String,
    md5: String,
    filename: String,
    filesize: i64,
    mtime: i64,
    /// Set once the bytes have been verified and stored, so registration can't
    /// commit metadata for an object that was never uploaded.
    uploaded: bool,
    created: std::time::Instant,
}

/// An unguessable upload token (128 bits of OS randomness, hex-encoded). `None`
/// if the OS RNG can't be read, so the caller can fail the request rather than
/// panic.
fn upload_token() -> Option<String> {
    use std::io::Read;
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .ok()?;
    Some(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Maximum buffered request body. Attachments can be large; everything else is
/// tiny. A finite cap bounds per-request memory so one device can't OOM the host
/// (the body is fully buffered by the auth middleware before handlers run).
const MAX_BODY: usize = 256 * 1024 * 1024;

/// Item keys become object keys in the bucket (and path components in URLs), so
/// reject anything that isn't a plain alphanumeric token (no `/`, `.`, `..`).
/// Zotero keys are 8 alphanumeric chars; allow a little slack.
fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 32 && key.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn cfg() -> &'static Config {
    CFG.get().expect("config initialised in main")
}

fn pool() -> &'static PgPool {
    POOL.get().expect("pool initialised in main")
}

fn storage() -> &'static s3::Storage {
    STORAGE.get().expect("storage initialised in main")
}

/// Access descriptor for the single configured user; no groups. `write` reflects
/// the requesting key, so a read-only key reports `write: false`.
fn access_payload(write: bool) -> Value {
    json!({
        "user": { "library": true, "files": true, "notes": true, "write": write },
        "groups": {}
    })
}

/// The access the request's key carries, if it presents a known one.
fn key_access(headers: &HeaderMap) -> Option<Access> {
    headers
        .get("zotero-api-key")
        .and_then(|v| v.to_str().ok())
        .and_then(|token| cfg().keys.get(token).copied())
}

fn version_headers(version: i64) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "last-modified-version",
        version.to_string().parse().unwrap(),
    );
    headers
}

async fn current_headers() -> HeaderMap {
    version_headers(store::current_version(pool()).await.unwrap_or(0))
}

/// For a since/versions read: the current library version, and whether the
/// client already holds everything up to it (→ `304 Not Modified`; nothing has a
/// version greater than `since`). `since == 0` is the initial pull, so never
/// 304 it. One DB read, so the caller reuses `current` for the response's
/// `Last-Modified-Version` instead of querying it again.
async fn since_check(since: i64) -> (i64, bool) {
    let current = store::current_version(pool()).await.unwrap_or(0);
    (current, since > 0 && since >= current)
}

/// The `If-Modified-Since-Version` request header (0 if absent/unparseable). The
/// client uses it on reads that don't carry a `since` query param (e.g. settings).
fn if_modified_since(headers: &HeaderMap) -> i64 {
    headers
        .get("if-modified-since-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Build a header value from stored/derived text without panicking on a stray
/// byte (e.g. a malformed md5); an invalid value is dropped rather than 500ing.
fn header_value(text: &str) -> axum::http::HeaderValue {
    text.parse()
        .unwrap_or_else(|_| axum::http::HeaderValue::from_static(""))
}

// --- authentication & login session ---------------------------------------

async fn key_current(headers: HeaderMap) -> Response {
    let write = key_access(&headers).is_some_and(|a| a.write);
    Json(json!({
        "userID": cfg().user_id,
        "username": "zhost",
        "displayName": "zhost",
        "access": access_payload(write),
    }))
    .into_response()
}

/// Zotero's "Login" uses a browser-authorised session rather than credentials:
/// the client opens `loginURL` in the user's browser, then polls the session
/// until it reports `status: "completed"` with a key. Mint a pending session and
/// point `loginURL` at our `/login` (which the user must pass an SSO gate to
/// reach); the key is withheld until that authorises the session.
async fn create_session() -> Response {
    let Some(token) = upload_token() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    {
        let mut sessions = SESSIONS.lock().unwrap();
        sessions.retain(|_, s| s.created.elapsed() < SESSION_TTL);
        sessions.insert(
            token.clone(),
            LoginSession {
                authorized: false,
                created: std::time::Instant::now(),
            },
        );
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "sessionToken": token,
            "loginURL": format!("{}/login?session={}", cfg().public_url, token),
        })),
    )
        .into_response()
}

/// Poll a login session: hand out the key only once `/login` has authorised it,
/// otherwise report it still pending (so an unauthorised or unknown token never
/// yields a key).
async fn check_session(Path(token): Path<String>) -> Response {
    let authorized = {
        let sessions = SESSIONS.lock().unwrap();
        sessions
            .get(&token)
            .is_some_and(|s| s.authorized && s.created.elapsed() < SESSION_TTL)
    };
    if authorized {
        Json(json!({
            "status": "completed",
            "apiKey": cfg().login_key,
            "userID": cfg().user_id,
            "username": "zhost",
        }))
        .into_response()
    } else {
        Json(json!({ "status": "pending" })).into_response()
    }
}

async fn cancel_session(Path(token): Path<String>) -> StatusCode {
    SESSIONS.lock().unwrap().remove(&token);
    StatusCode::NO_CONTENT
}

/// The login consent page. The user reaches it from `loginURL` in their browser
/// (behind the SSO gate in production). It does **not** authorise on its own — it
/// renders a form that POSTs back to confirm. Splitting render (GET) from action
/// (POST) stops a prefetch or a cross-site `GET …/login?session=…` from silently
/// authorising a session, which would be a confused-deputy key grant: the
/// attacker creates the session (so knows its token) and only needs an
/// authenticated browser to hit the URL.
async fn login_page(Query(params): Query<HashMap<String, String>>) -> Response {
    let Some(token) = params.get("session") else {
        return (StatusCode::BAD_REQUEST, "missing session").into_response();
    };
    let known = {
        let sessions = SESSIONS.lock().unwrap();
        sessions
            .get(token)
            .is_some_and(|s| s.created.elapsed() < SESSION_TTL)
    };
    if !known {
        return (StatusCode::NOT_FOUND, "unknown or expired session").into_response();
    }
    // The token is server-minted hex, safe to interpolate into the hidden field.
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>zhost login</title>\
         <h1>Authorize this Zotero login?</h1>\
         <form method=post action=\"/login\">\
         <input type=hidden name=session value=\"{token}\">\
         <button type=submit>Approve</button></form>"
    );
    axum::response::Html(body).into_response()
}

/// Authorise the session the consent form submits. Reaching this means the
/// request cleared the SSO gate; additionally reject a cross-site form post by
/// requiring `Origin` (when the browser sends it) to be our own, so an
/// authenticated user's browser can't be steered into authorising someone else's
/// session. A request with no `Origin` (a CLI, not a browser) is allowed.
async fn login_authorize(
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        if origin.trim_end_matches('/') != cfg().public_url.trim_end_matches('/') {
            return (StatusCode::FORBIDDEN, "bad origin").into_response();
        }
    }
    // When an authorized user is configured, the front SSO proxy must forward a
    // matching identity (oauth2-proxy `--set-xauthrequest`). This ties the
    // approval to a proven identity and guards against a misconfigured/bypassed
    // proxy; unset (a private network) means the network is the gate.
    if let Some(want) = &cfg().login_authorized_user {
        let identity = headers
            .get("x-auth-request-email")
            .or_else(|| headers.get("x-auth-request-user"))
            .and_then(|v| v.to_str().ok());
        if !identity.is_some_and(|got| got.eq_ignore_ascii_case(want)) {
            return (StatusCode::FORBIDDEN, "not an authorized user").into_response();
        }
    }
    let Some(token) = form.get("session") else {
        return (StatusCode::BAD_REQUEST, "missing session").into_response();
    };
    let mut sessions = SESSIONS.lock().unwrap();
    match sessions.get_mut(token) {
        Some(s) if s.created.elapsed() < SESSION_TTL => {
            s.authorized = true;
            (StatusCode::OK, "Authorized — return to Zotero.").into_response()
        }
        _ => (StatusCode::NOT_FOUND, "unknown or expired session").into_response(),
    }
}

// --- library data -----------------------------------------------------------

async fn groups() -> Response {
    (current_headers().await, Json(json!({}))).into_response()
}

fn server_error(context: &str, error: sqlx::Error) -> Response {
    tracing::error!(%error, context, "database error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn s3_error(context: &str, error: s3::S3Error) -> Response {
    tracing::error!(%error, context, "object storage error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// The library version the client expects to still hold; a mismatch is a 412.
fn if_unmodified(headers: &HeaderMap) -> Option<i64> {
    headers
        .get("if-unmodified-since-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

/// A mutating data write must carry a parseable `If-Unmodified-Since-Version`.
/// Without it the version guard is bypassed (a missing/garbage header would let
/// the write commit unconditionally), so reject it with `428 Precondition
/// Required` — the same contract the file endpoints use.
// The Err is a full Response (the idiomatic axum guard shape); it's only built
// on the rare rejection path, so the large-Err size is fine.
#[allow(clippy::result_large_err)]
fn precondition(headers: &HeaderMap) -> Result<i64, Response> {
    if_unmodified(headers).ok_or_else(|| {
        (
            StatusCode::PRECONDITION_REQUIRED,
            "If-Unmodified-Since-Version required",
        )
            .into_response()
    })
}

fn conflict(current: i64) -> Response {
    (StatusCode::PRECONDITION_FAILED, version_headers(current)).into_response()
}

/// The `since` read cursor (defaults to 0, the initial pull).
fn since_of(params: &HashMap<String, String>) -> i64 {
    params
        .get("since")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// A comma-separated key list parameter (e.g. `itemKey=a,b`), empty if absent.
fn csv_of(params: &HashMap<String, String>, key: &str) -> Vec<String> {
    params
        .get(key)
        .map(|csv| csv.split(',').map(String::from).collect())
        .unwrap_or_default()
}

/// `format=versions&since=N` returns the changed `{key: version}` map; otherwise
/// `?<kind>Key=a,b&format=json` returns the full `[{key, version, data}]`.
async fn read(kind: &str, params: HashMap<String, String>) -> Response {
    if params.get("format").map(String::as_str) == Some("versions") {
        let since = since_of(&params);
        // Always 200 with the (possibly empty) versions map. The client's
        // `getVersions` sends no `If-Modified-Since-Version` header and treats a
        // 304 as "no data", which then mismatches its library-version check and
        // makes it restart the sync forever. 304 is only for the header path
        // (settings), not for `?since=` versions reads.
        let current = store::current_version(pool()).await.unwrap_or(0);
        return match store::versions(pool(), kind, since).await {
            Ok(value) => (version_headers(current), Json(value)).into_response(),
            Err(error) => server_error("read", error),
        };
    }
    let keys = csv_of(&params, &format!("{kind}Key"));
    match store::objects(pool(), kind, &keys).await {
        Ok(value) => (current_headers().await, Json(value)).into_response(),
        Err(error) => server_error("read", error),
    }
}

/// The two sync reads shared by `/items` and `/items/top`: the `format=versions`
/// map and the `?itemKey=…` batch. With `top`, the versions map is restricted to
/// top-level items (the client's parent-first phase). Returns `None` when the
/// request carries neither, i.e. it is a CLI query rather than a sync read.
async fn item_sync_read(params: &query::Params, top: bool) -> Option<Response> {
    if params.get("format") == Some("versions") {
        let since = params
            .get("since")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        // Always 200 + map (never 304); see `read` — a 304 here loops the client.
        let current = store::current_version(pool()).await.unwrap_or(0);
        let result = if top {
            store::top_versions(pool(), since).await
        } else {
            store::versions(pool(), "item", since).await
        };
        return Some(match result {
            Ok(value) => (version_headers(current), Json(value)).into_response(),
            Err(error) => server_error("items versions", error),
        });
    }
    if let Some(csv) = params.get("itemKey") {
        let keys: Vec<String> = csv.split(',').map(String::from).collect();
        return Some(match store::objects(pool(), "item", &keys).await {
            Ok(value) => (current_headers().await, Json(value)).into_response(),
            Err(error) => server_error("items batch", error),
        });
    }
    None
}

/// Render an item query as a paged JSON listing: the `[{key, version, data}]`
/// array plus `Total-Results` and, while more rows remain, a `Link: …;
/// rel="next"` built against `path` (the public-URL endpoint).
async fn item_listing(path: &str, raw: Option<&str>, q: &query::ItemQuery) -> Response {
    match store::query_items(pool(), q).await {
        Ok((items, total)) => {
            let mut headers = current_headers().await;
            headers.insert("total-results", total.to_string().parse().unwrap());
            if q.start + q.limit < total {
                let link = next_link(path, raw, q.start + q.limit);
                headers.insert("link", link.parse().unwrap());
            }
            (headers, Json(Value::Array(items))).into_response()
        }
        Err(error) => server_error("items query", error),
    }
}

/// The `Link: <…>; rel="next"` header for the page after `start`, preserving the
/// request's other params and pointing at the public (reverse-proxy) URL.
fn next_link(path: &str, raw: Option<&str>, start: i64) -> String {
    let mut pairs: Vec<(String, String)> = raw
        .and_then(|q| serde_urlencoded::from_str(q).ok())
        .unwrap_or_default();
    pairs.retain(|(k, _)| k != "start");
    pairs.push(("start".into(), start.to_string()));
    let qs = serde_urlencoded::to_string(&pairs).unwrap_or_default();
    format!("<{}{}?{}>; rel=\"next\"", cfg().public_url, path, qs)
}

/// `format=keys` returns every matching item key (no paging) as a plain-text
/// newline list — the shape Zotero's `getKeys()` parses (it reads the body as
/// `responseText.split('\n')`). Returns `None` for any other format.
async fn item_keys_response(params: &query::Params, q: &query::ItemQuery) -> Option<Response> {
    if params.get("format") != Some("keys") {
        return None;
    }
    Some(match store::item_keys(pool(), q).await {
        // A `String` body sets `Content-Type: text/plain`, which is what the
        // client expects; current_headers adds `Last-Modified-Version`.
        Ok(keys) => (current_headers().await, keys.join("\n")).into_response(),
        Err(error) => server_error("item keys", error),
    })
}

/// `GET /users/<id>/items`: the two sync reads, or the CLI query when neither.
async fn items_get(Path(id): Path<String>, RawQuery(raw): RawQuery) -> Response {
    let params = query::Params::parse(raw.as_deref());
    if let Some(resp) = item_sync_read(&params, false).await {
        return resp;
    }
    let q = query::ItemQuery::from_params(&params);
    if let Some(resp) = item_keys_response(&params, &q).await {
        return resp;
    }
    item_listing(&format!("/users/{id}/items"), raw.as_deref(), &q).await
}

/// `GET /users/<id>/items/top`: top-level items (no `parentItem`). Also answers
/// the sync `format=versions` (top-filtered) and `itemKey` reads sent here.
async fn items_top(Path(id): Path<String>, RawQuery(raw): RawQuery) -> Response {
    let params = query::Params::parse(raw.as_deref());
    if let Some(resp) = item_sync_read(&params, true).await {
        return resp;
    }
    let mut q = query::ItemQuery::from_params(&params);
    q.top = true;
    if let Some(resp) = item_keys_response(&params, &q).await {
        return resp;
    }
    item_listing(&format!("/users/{id}/items/top"), raw.as_deref(), &q).await
}

/// `GET /users/<id>/items/trash`: only trashed items (`data.deleted`).
async fn items_trash(Path(id): Path<String>, RawQuery(raw): RawQuery) -> Response {
    let params = query::Params::parse(raw.as_deref());
    let mut q = query::ItemQuery::from_params(&params);
    q.only_trashed = true;
    if let Some(resp) = item_keys_response(&params, &q).await {
        return resp;
    }
    item_listing(&format!("/users/{id}/items/trash"), raw.as_deref(), &q).await
}

/// `GET /users/<id>/collections/<key>/items`: items in the given collection.
async fn collection_items(
    Path((id, key)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = query::Params::parse(raw.as_deref());
    let mut q = query::ItemQuery::from_params(&params);
    q.collection = Some(key.clone());
    if let Some(resp) = item_keys_response(&params, &q).await {
        return resp;
    }
    item_listing(
        &format!("/users/{id}/collections/{key}/items"),
        raw.as_deref(),
        &q,
    )
    .await
}

/// `GET /users/<id>/collections/<key>/items/top`: top-level items in the
/// collection. The sync client requests this with `format=keys` when restoring a
/// previously-deleted collection (syncEngine.js `_restoreRestoredCollectionItems`).
async fn collection_items_top(
    Path((id, key)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Response {
    let params = query::Params::parse(raw.as_deref());
    let mut q = query::ItemQuery::from_params(&params);
    q.collection = Some(key.clone());
    q.top = true;
    if let Some(resp) = item_keys_response(&params, &q).await {
        return resp;
    }
    item_listing(
        &format!("/users/{id}/collections/{key}/items/top"),
        raw.as_deref(),
        &q,
    )
    .await
}

/// `GET /users/<id>/tags`: distinct tags with item counts.
async fn tags_get() -> Response {
    match store::tags(pool()).await {
        Ok(value) => (current_headers().await, Json(value)).into_response(),
        Err(error) => server_error("tags", error),
    }
}

/// `merge` distinguishes a `PATCH` (partial update, merge into the stored object)
/// from a `POST` (full replace).
async fn write(kind: &str, headers: HeaderMap, body: Bytes, merge: bool) -> Response {
    let batch: Vec<Value> = match serde_json::from_slice(&body) {
        Ok(batch) => batch,
        Err(error) => {
            tracing::warn!(%error, "malformed write body");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let expected = match precondition(&headers) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match store::write(pool(), kind, batch, Some(expected), merge).await {
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
        Err(error) => server_error("write", error),
    }
}

async fn delete(kind: &str, headers: HeaderMap, params: HashMap<String, String>) -> Response {
    let expected = match precondition(&headers) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let keys = csv_of(&params, &format!("{kind}Key"));
    match store::delete(pool(), kind, &keys, Some(expected)).await {
        Ok(store::Outcome::Done(version)) => {
            (StatusCode::NO_CONTENT, version_headers(version)).into_response()
        }
        Ok(store::Outcome::Conflict(current)) => conflict(current),
        Err(error) => server_error("delete", error),
    }
}

async fn settings_read(
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    // The client may send the cursor as ?since= or the If-Modified-Since-Version
    // header; honour whichever is higher.
    let since = since_of(&params).max(if_modified_since(&headers));
    let (current, fresh) = since_check(since).await;
    if fresh {
        return (StatusCode::NOT_MODIFIED, version_headers(current)).into_response();
    }
    match store::settings(pool()).await {
        Ok(value) => (version_headers(current), Json(value)).into_response(),
        Err(error) => server_error("settings", error),
    }
}

async fn settings_write(headers: HeaderMap, body: Bytes) -> Response {
    let expected = match precondition(&headers) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let value: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    match store::write_settings(pool(), value, Some(expected)).await {
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
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let expected = match precondition(&headers) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let keys = csv_of(&params, "settingKey");
    match store::delete_settings(pool(), &keys, Some(expected)).await {
        Ok(store::Outcome::Done(version)) => {
            (StatusCode::NO_CONTENT, version_headers(version)).into_response()
        }
        Ok(store::Outcome::Conflict(current)) => conflict(current),
        Err(error) => server_error("settings delete", error),
    }
}

async fn deleted(Query(params): Query<HashMap<String, String>>) -> Response {
    let since = since_of(&params);
    match store::deleted(pool(), since).await {
        Ok(value) => (current_headers().await, Json(value)).into_response(),
        Err(error) => server_error("deleted", error),
    }
}

/// `GET /fulltext?format=versions&since=N` → `{itemKey: version}` for content
/// changed after `since`, so the client downloads only what it lacks.
async fn fulltext_versions(Query(params): Query<HashMap<String, String>>) -> Response {
    let since = since_of(&params);
    // Always 200 + map (never 304); see `read` — a 304 here loops the client.
    let current = store::current_version(pool()).await.unwrap_or(0);
    match store::fulltext_versions(pool(), since).await {
        Ok(value) => (version_headers(current), Json(value)).into_response(),
        Err(error) => server_error("fulltext versions", error),
    }
}

/// `GET /items/<key>/fulltext` → the item's content object, with the row's
/// version in `Last-Modified-Version` (the client stores it to skip re-fetching).
async fn fulltext_item(Path((_id, key)): Path<(String, String)>) -> Response {
    match store::fulltext_item(pool(), &key).await {
        Ok(Some((version, data))) => (version_headers(version), Json(data)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => server_error("fulltext item", error),
    }
}

/// `POST /fulltext` — store a batch of extracted content, returning the per-index
/// result map the client reads to mark each item synced (or `412` if stale).
async fn fulltext_write(headers: HeaderMap, body: Bytes) -> Response {
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
    match store::write_fulltext(pool(), batch, Some(expected)).await {
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
        let pending = PENDING.lock().unwrap().get(token).cloned();
        let Some(upload) = pending else {
            return (StatusCode::BAD_REQUEST, "no pending upload").into_response();
        };
        if upload.item_key != key {
            return (StatusCode::BAD_REQUEST, "upload token does not match item").into_response();
        }
        if !upload.uploaded {
            return (StatusCode::BAD_REQUEST, "no uploaded bytes").into_response();
        }
        return match store::register_file(
            pool(),
            &key,
            &upload.md5,
            &upload.filename,
            upload.filesize,
            upload.mtime,
        )
        .await
        {
            Ok(version) => {
                PENDING.lock().unwrap().remove(token);
                (StatusCode::NO_CONTENT, version_headers(version)).into_response()
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

    let md5 = form.get("md5").cloned().unwrap_or_default();
    let stored_md5 = match store::file_meta(pool(), &key).await {
        Ok(meta) => meta.map(|(m, _)| m),
        Err(error) => return server_error("file auth", error),
    };

    // md5 hex compares case-insensitively, matching the verification in
    // upload_put (which lowercases) so dedup and replace agree on normalization.
    if if_none_match {
        // "Only if no file exists." Same md5 → already uploaded (dedup); a
        // different existing file → conflict.
        if let Some(existing) = &stored_md5 {
            if existing.eq_ignore_ascii_case(&md5) {
                return (current_headers().await, Json(json!({ "exists": 1 }))).into_response();
            }
            return conflict(store::current_version(pool()).await.unwrap_or(0));
        }
    } else if let Some(want) = &if_match {
        // "Only if the current md5 matches." Otherwise → conflict.
        if !stored_md5
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case(want))
        {
            return conflict(store::current_version(pool()).await.unwrap_or(0));
        }
    }

    // Authorize: mint an unguessable token, remember the upload (pruning stale
    // ones), and hand back the upload URL. The bytes land at the item key's path.
    let Some(token) = upload_token() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    {
        let mut pending = PENDING.lock().unwrap();
        pending.retain(|_, u| u.created.elapsed() < PENDING_TTL);
        pending.insert(
            token.clone(),
            PendingUpload {
                item_key: key.clone(),
                md5,
                filename: form.get("filename").cloned().unwrap_or_default(),
                filesize: form
                    .get("filesize")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                mtime: form.get("mtime").and_then(|s| s.parse().ok()).unwrap_or(0),
                uploaded: false,
                created: std::time::Instant::now(),
            },
        );
    }
    // Empty prefix/suffix: the client PUTs the raw file bytes to url.
    Json(json!({
        "url": format!("{}/uploads/{}", cfg().public_url, token),
        "uploadKey": token,
        "contentType": "application/octet-stream",
        "prefix": "",
        "suffix": "",
    }))
    .into_response()
}

/// Receive the raw attachment bytes for a pending upload token, verify them
/// against the authorized md5/filesize, and store the object in the bucket.
/// Rejects an unknown token. Verifying here (where the bytes are in hand) keeps
/// the integrity check server-side now that the bytes go straight to S3.
async fn upload_put(Path(token): Path<String>, body: Bytes) -> Response {
    let pending = PENDING.lock().unwrap().get(&token).cloned();
    let Some(upload) = pending else {
        return (StatusCode::BAD_REQUEST, "unknown upload token").into_response();
    };
    let actual_md5 = {
        use md5::{Digest, Md5};
        format!("{:x}", Md5::new().chain_update(&body).finalize())
    };
    if body.len() as i64 != upload.filesize || actual_md5 != upload.md5.to_lowercase() {
        tracing::warn!(
            key = upload.item_key,
            want_md5 = upload.md5,
            got_md5 = actual_md5,
            "uploaded bytes do not match authorization"
        );
        return (
            StatusCode::BAD_REQUEST,
            "uploaded bytes do not match md5/filesize",
        )
            .into_response();
    }
    if let Err(error) = storage()
        .put(&upload.item_key, &body, "application/octet-stream")
        .await
    {
        return s3_error("store file", error);
    }
    // Mark the pending upload stored so registration can commit its metadata.
    if let Some(u) = PENDING.lock().unwrap().get_mut(&token) {
        u.uploaded = true;
    }
    StatusCode::CREATED.into_response()
}

/// The client reads md5/mtime from this response's headers and then downloads
/// the bytes from `Location` — a short-lived pre-signed GET URL pointing straight
/// at the bucket, so the read path bypasses this server entirely (and the URL is
/// an unguessable, expiring capability the client follows without an API key).
async fn file_get(Path((_id, key)): Path<(String, String)>) -> Response {
    if !valid_key(&key) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (md5, mtime) = match store::file_meta(pool(), &key).await {
        Ok(Some(meta)) => meta,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return server_error("file meta", error),
    };
    let url = match storage().presign_get(&key).await {
        Ok(url) => url,
        Err(error) => return s3_error("presign download", error),
    };
    let mut headers = HeaderMap::new();
    headers.insert("location", header_value(&url));
    headers.insert(
        "zotero-file-modification-time",
        header_value(&mtime.to_string()),
    );
    headers.insert("zotero-file-md5", header_value(&md5));
    headers.insert("zotero-file-compressed", header_value("No"));
    (StatusCode::FOUND, headers).into_response()
}

// --- middleware -------------------------------------------------------------

/// Decode gzip write bodies, log the request, and reject anything without the
/// configured key except the bootstrap (key/session creation, login) endpoints.
async fn log_and_auth(req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let raw = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(raw) => raw,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };

    let gzipped = parts
        .headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|e| e.contains("gzip"));
    let bytes = if gzipped {
        use std::io::Read;
        // Cap the decompressed size too, so a small gzip can't expand without
        // bound (a malformed/over-large body then fails to parse downstream).
        let mut decoder = flate2::read::GzDecoder::new(&raw[..]).take(MAX_BODY as u64);
        let mut out = Vec::new();
        match decoder.read_to_end(&mut out) {
            Ok(_) => {
                parts.headers.remove("content-encoding");
                parts.headers.remove("content-length");
                Bytes::from(out)
            }
            Err(_) => raw,
        }
    } else {
        raw
    };

    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_string()
    };
    let method = parts.method.clone();
    let uri = parts.uri.clone();
    tracing::info!(
        %method,
        %uri,
        api_version = %header("zotero-api-version"),
        if_unmod = %header("if-unmodified-since-version"),
        body = %String::from_utf8_lossy(&bytes).chars().take(400).collect::<String>(),
        "request"
    );

    let path = parts.uri.path();
    let is_bootstrap =
        path.starts_with("/keys/sessions") || path.starts_with("/uploads") || path == "/login";
    if !is_bootstrap {
        let Some(access) = key_access(&parts.headers) else {
            return (StatusCode::FORBIDDEN, "invalid API key").into_response();
        };
        let mutating = matches!(
            parts.method,
            axum::http::Method::POST | axum::http::Method::PATCH | axum::http::Method::DELETE
        );
        if mutating && !access.write {
            return (StatusCode::FORBIDDEN, "read-only API key").into_response();
        }
    }

    let response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        tracing::warn!(%method, %uri, status = status.as_u16(), "response error");
    }
    response
}

fn app() -> Router {
    // Each object kind shares the read/write/delete logic; the closures bind the
    // kind so the handlers stay generic.
    let objects = |kind: &'static str| {
        get(move |Query(p): Query<HashMap<String, String>>| read(kind, p))
            .post(move |headers: HeaderMap, body: Bytes| write(kind, headers, body, false))
            .patch(move |headers: HeaderMap, body: Bytes| write(kind, headers, body, true))
            .delete(
                move |headers: HeaderMap, Query(p): Query<HashMap<String, String>>| {
                    delete(kind, headers, p)
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
                .post(move |headers: HeaderMap, body: Bytes| write("item", headers, body, false))
                .patch(move |headers: HeaderMap, body: Bytes| write("item", headers, body, true))
                .delete(
                    move |headers: HeaderMap, Query(p): Query<HashMap<String, String>>| {
                        delete("item", headers, p)
                    },
                ),
        )
        .route("/users/{id}/items/top", get(items_top))
        .route("/users/{id}/items/trash", get(items_trash))
        .route("/users/{id}/tags", get(tags_get))
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
        .layer(middleware::from_fn(log_and_auth))
}

/// Build the token→access map from secret files. `ZHOST_KEYS` is a
/// comma-separated list of `<role>:<path>` entries (`rw`/`ro`), each path a
/// single-line token (a sops-nix secret exposed via systemd LoadCredential).
/// Falls back to a single read/write key from `ZHOST_API_KEY_FILE` /
/// `ZHOST_API_KEY` for simple deployments. Returns the map and a read/write
/// token to hand the app through the login session. Prefer files over the env,
/// which is visible in /proc.
fn load_keys() -> (HashMap<String, Access>, String) {
    let read_token = |path: &str| {
        std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read key file {path}: {e}"))
            .trim()
            .to_string()
    };
    let mut keys = HashMap::new();
    let mut login_key = None;

    if let Ok(manifest) = std::env::var("ZHOST_KEYS") {
        for entry in manifest.split(',').filter(|s| !s.is_empty()) {
            let (role, path) = entry
                .split_once(':')
                .unwrap_or_else(|| panic!("ZHOST_KEYS entry not <role>:<path>: {entry}"));
            let write = role == "rw";
            let token = read_token(path);
            if write && login_key.is_none() {
                login_key = Some(token.clone());
            }
            keys.insert(token, Access { write });
        }
    } else {
        let token = match std::env::var("ZHOST_API_KEY_FILE") {
            Ok(path) => read_token(&path),
            Err(_) => std::env::var("ZHOST_API_KEY").unwrap_or_else(|_| "zhost-dev-key".into()),
        };
        login_key = Some(token.clone());
        keys.insert(token, Access { write: true });
    }

    (
        keys,
        login_key.expect("at least one read/write key configured"),
    )
}

/// Object storage settings from the environment. The access/secret keys prefer
/// a file (`*_FILE`, a systemd credential) over the raw env var, which is
/// visible in /proc — the same precedence as the API keys. `path_style` defaults
/// on (required by RustFS/MinIO, accepted by R2); `region` defaults to `auto`
/// (R2 ignores it). Defaults target a local RustFS for development.
fn load_s3() -> s3::Config {
    let from_file_or_env = |file: &str, var: &str| {
        std::env::var(file)
            .ok()
            .map(|path| {
                std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read S3 key file {path}: {e}"))
                    .trim()
                    .to_string()
            })
            .or_else(|| std::env::var(var).ok())
            .unwrap_or_default()
    };
    s3::Config {
        endpoint: std::env::var("ZHOST_S3_ENDPOINT")
            .unwrap_or_else(|_| "http://127.0.0.1:9000".into()),
        region: std::env::var("ZHOST_S3_REGION").unwrap_or_else(|_| "auto".into()),
        bucket: std::env::var("ZHOST_S3_BUCKET").unwrap_or_else(|_| "zotero".into()),
        access_key: from_file_or_env("ZHOST_S3_ACCESS_KEY_FILE", "ZHOST_S3_ACCESS_KEY"),
        secret_key: from_file_or_env("ZHOST_S3_SECRET_KEY_FILE", "ZHOST_S3_SECRET_KEY"),
        path_style: std::env::var("ZHOST_S3_PATH_STYLE")
            .map(|v| v != "false")
            .unwrap_or(true),
        // Short by default: the client follows the download redirect right away,
        // so the URL needn't stay valid long (it is an unauthenticated capability).
        presign_ttl: std::env::var("ZHOST_S3_PRESIGN_TTL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let (keys, login_key) = load_keys();
    let bind = std::env::var("ZHOST_BIND").unwrap_or_else(|_| "127.0.0.1:8189".into());
    let _ = CFG.set(Config {
        keys,
        login_key,
        user_id: std::env::var("ZHOST_USER_ID")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1),
        public_url: std::env::var("ZHOST_PUBLIC_URL").unwrap_or_else(|_| format!("http://{bind}")),
        bind,
        database_url: std::env::var("ZHOST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .unwrap_or_else(|_| "postgres://localhost/zhost".into()),
        s3: load_s3(),
        login_authorized_user: std::env::var("ZHOST_LOGIN_AUTHORIZED_USER")
            .ok()
            .filter(|s| !s.is_empty()),
    });

    let pool = store::connect(&cfg().database_url)
        .await
        .expect("connect to database");
    let _ = POOL.set(pool);

    let _ = STORAGE.set(s3::Storage::new(&cfg().s3).expect("init object storage"));

    let listener = tokio::net::TcpListener::bind(&cfg().bind)
        .await
        .expect("bind address");
    tracing::info!(bind = %cfg().bind, "zhost listening");
    axum::serve(listener, app())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server run");
}
