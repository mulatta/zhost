//! Self-hosted Zotero Web API v3 sync server.
//!
//! Objects are stored as opaque jsonb blobs in PostgreSQL (see `store`); each
//! write bumps a single library version counter so the client's `since` reads
//! and `If-Unmodified-Since-Version` writes stay coherent. See SPEC.md for the
//! protocol contract.

mod query;
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
    /// login, upload and download URLs handed to the client, which must be
    /// reachable by it — not the internal bind address.
    public_url: String,
    database_url: String,
    storage_dir: String,
}

static CFG: OnceLock<Config> = OnceLock::new();
static POOL: OnceLock<PgPool> = OnceLock::new();

/// In-flight file uploads, keyed by the upload key (= attachment item key),
/// remembered between the authorisation and registration steps.
static PENDING: LazyLock<Mutex<HashMap<String, PendingUpload>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
struct PendingUpload {
    md5: String,
    filename: String,
    filesize: i64,
    mtime: i64,
}

fn cfg() -> &'static Config {
    CFG.get().expect("config initialised in main")
}

fn pool() -> &'static PgPool {
    POOL.get().expect("pool initialised in main")
}

/// Access descriptor for the single configured user; no groups. `write` reflects
/// the requesting key, so a read-only key reports `write: false`.
fn access(write: bool) -> Value {
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

// --- authentication & login session ---------------------------------------

async fn create_key() -> Response {
    (
        StatusCode::CREATED,
        Json(json!({
            "key": cfg().login_key,
            "userID": cfg().user_id,
            "username": "zhost",
            "displayName": "zhost",
            "access": access(true),
        })),
    )
        .into_response()
}

async fn key_current(headers: HeaderMap) -> Response {
    let write = key_access(&headers).is_some_and(|a| a.write);
    Json(json!({
        "userID": cfg().user_id,
        "username": "zhost",
        "displayName": "zhost",
        "access": access(write),
    }))
    .into_response()
}

/// Zotero's "Login" uses a browser-authorised session rather than credentials:
/// the client opens `loginURL`, then polls the session until it reports
/// `status: "completed"` with a key. Single user, so we authorise immediately.
async fn create_session() -> Response {
    (
        StatusCode::CREATED,
        Json(json!({
            "sessionToken": "zhost-session",
            "loginURL": format!("{}/login", cfg().public_url),
        })),
    )
        .into_response()
}

async fn check_session() -> Response {
    Json(json!({
        "status": "completed",
        "apiKey": cfg().login_key,
        "userID": cfg().user_id,
        "username": "zhost",
    }))
    .into_response()
}

async fn cancel_session() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn login_page() -> &'static str {
    "Authorized — return to Zotero."
}

// --- library data -----------------------------------------------------------

async fn groups() -> Response {
    (current_headers().await, Json(json!({}))).into_response()
}

fn server_error(context: &str, error: sqlx::Error) -> Response {
    tracing::error!(%error, context, "database error");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// The library version the client expects to still hold; a mismatch is a 412.
fn if_unmodified(headers: &HeaderMap) -> Option<i64> {
    headers
        .get("if-unmodified-since-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

fn conflict(current: i64) -> Response {
    (StatusCode::PRECONDITION_FAILED, version_headers(current)).into_response()
}

/// `format=versions&since=N` returns the changed `{key: version}` map; otherwise
/// `?<kind>Key=a,b&format=json` returns the full `[{key, version, data}]`.
async fn read(kind: &str, params: HashMap<String, String>) -> Response {
    let result = if params.get("format").map(String::as_str) == Some("versions") {
        let since = params
            .get("since")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        store::versions(pool(), kind, since).await
    } else {
        let keys = params
            .get(&format!("{kind}Key"))
            .map(|csv| csv.split(',').map(String::from).collect::<Vec<_>>())
            .unwrap_or_default();
        store::objects(pool(), kind, &keys).await
    };
    match result {
        Ok(value) => (current_headers().await, Json(value)).into_response(),
        Err(error) => server_error("read", error),
    }
}

/// The two sync reads shared by `/items` and `/items/top`: the `format=versions`
/// map and the `?itemKey=…` batch. Returns `None` when the request carries
/// neither, i.e. it is a CLI query rather than a sync read.
async fn item_sync_read(params: &query::Params) -> Option<Response> {
    if params.get("format") == Some("versions") {
        let since = params
            .get("since")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        return Some(match store::versions(pool(), "item", since).await {
            Ok(value) => (current_headers().await, Json(value)).into_response(),
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

/// `GET /users/<id>/items`: the two sync reads, or the CLI query when neither.
async fn items_get(Path(id): Path<String>, RawQuery(raw): RawQuery) -> Response {
    let params = query::Params::parse(raw.as_deref());
    if let Some(resp) = item_sync_read(&params).await {
        return resp;
    }
    let q = query::ItemQuery::from_params(&params);
    item_listing(&format!("/users/{id}/items"), raw.as_deref(), &q).await
}

/// `GET /users/<id>/items/top`: top-level items (no `parentItem`). Still answers
/// the sync `format=versions`/`itemKey` reads the client may send here.
async fn items_top(Path(id): Path<String>, RawQuery(raw): RawQuery) -> Response {
    let params = query::Params::parse(raw.as_deref());
    if let Some(resp) = item_sync_read(&params).await {
        return resp;
    }
    let mut q = query::ItemQuery::from_params(&params);
    q.top = true;
    item_listing(&format!("/users/{id}/items/top"), raw.as_deref(), &q).await
}

/// `GET /users/<id>/items/trash`: only trashed items (`data.deleted`).
async fn items_trash(Path(id): Path<String>, RawQuery(raw): RawQuery) -> Response {
    let mut q = query::ItemQuery::from_params(&query::Params::parse(raw.as_deref()));
    q.only_trashed = true;
    item_listing(&format!("/users/{id}/items/trash"), raw.as_deref(), &q).await
}

/// `GET /users/<id>/collections/<key>/items`: items in the given collection.
async fn collection_items(
    Path((id, key)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
) -> Response {
    let mut q = query::ItemQuery::from_params(&query::Params::parse(raw.as_deref()));
    q.collection = Some(key.clone());
    item_listing(
        &format!("/users/{id}/collections/{key}/items"),
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

async fn write(kind: &str, headers: HeaderMap, body: Bytes) -> Response {
    let batch: Vec<Value> = match serde_json::from_slice(&body) {
        Ok(batch) => batch,
        Err(error) => {
            tracing::warn!(%error, "malformed write body");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    match store::write(pool(), kind, batch, if_unmodified(&headers)).await {
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
    let keys = params
        .get(&format!("{kind}Key"))
        .map(|csv| csv.split(',').map(String::from).collect::<Vec<_>>())
        .unwrap_or_default();
    match store::delete(pool(), kind, &keys, if_unmodified(&headers)).await {
        Ok(store::Outcome::Done(version)) => {
            (StatusCode::NO_CONTENT, version_headers(version)).into_response()
        }
        Ok(store::Outcome::Conflict(current)) => conflict(current),
        Err(error) => server_error("delete", error),
    }
}

async fn settings_read() -> Response {
    match store::settings(pool()).await {
        Ok(value) => (current_headers().await, Json(value)).into_response(),
        Err(error) => server_error("settings", error),
    }
}

async fn settings_write(headers: HeaderMap, body: Bytes) -> Response {
    let value: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    match store::write_settings(pool(), value, if_unmodified(&headers)).await {
        Ok(store::Outcome::Done(version)) => {
            (StatusCode::NO_CONTENT, version_headers(version)).into_response()
        }
        Ok(store::Outcome::Conflict(current)) => conflict(current),
        Err(error) => server_error("settings write", error),
    }
}

async fn deleted(Query(params): Query<HashMap<String, String>>) -> Response {
    let since = params
        .get("since")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    match store::deleted(pool(), since).await {
        Ok(value) => (current_headers().await, Json(value)).into_response(),
        Err(error) => server_error("deleted", error),
    }
}

/// `GET /fulltext?format=versions&since=N` → `{itemKey: version}` for content
/// changed after `since`, so the client downloads only what it lacks.
async fn fulltext_versions(Query(params): Query<HashMap<String, String>>) -> Response {
    let since = params
        .get("since")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    match store::fulltext_versions(pool(), since).await {
        Ok(value) => (current_headers().await, Json(value)).into_response(),
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
    match store::write_fulltext(pool(), batch, if_unmodified(&headers)).await {
        Ok(store::Outcome::Done((version, successful))) => (
            version_headers(version),
            Json(json!({ "successful": successful, "unchanged": {}, "failed": {} })),
        )
            .into_response(),
        Ok(store::Outcome::Conflict(current)) => conflict(current),
        Err(error) => server_error("fulltext write", error),
    }
}

fn file_path(item_key: &str) -> std::path::PathBuf {
    std::path::Path::new(&cfg().storage_dir).join(item_key)
}

/// Attachment file endpoint. The same path serves both POST steps:
/// authorisation (`md5`/`filename`/`filesize`/`mtime` form) and registration
/// (`upload` form, after the bytes have been PUT to the upload URL).
async fn file_post(
    Path((_id, key)): Path<(String, String)>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if form.contains_key("upload") {
        let pending = PENDING.lock().unwrap().get(&key).cloned();
        let Some(upload) = pending else {
            return (StatusCode::BAD_REQUEST, "no pending upload").into_response();
        };
        match store::register_file(
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
                PENDING.lock().unwrap().remove(&key);
                (StatusCode::NO_CONTENT, version_headers(version)).into_response()
            }
            Err(error) => server_error("register file", error),
        }
    } else {
        let md5 = form.get("md5").cloned().unwrap_or_default();
        match store::file_exists(pool(), &key, &md5).await {
            Ok(true) => (current_headers().await, Json(json!({ "exists": 1 }))).into_response(),
            Ok(false) => {
                PENDING.lock().unwrap().insert(
                    key.clone(),
                    PendingUpload {
                        md5,
                        filename: form.get("filename").cloned().unwrap_or_default(),
                        filesize: form
                            .get("filesize")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0),
                        mtime: form.get("mtime").and_then(|s| s.parse().ok()).unwrap_or(0),
                    },
                );
                // Empty prefix/suffix: the client PUTs the raw file bytes to url.
                Json(json!({
                    "url": format!("{}/uploads/{}", cfg().public_url, key),
                    "uploadKey": key,
                    "contentType": "application/octet-stream",
                    "prefix": "",
                    "suffix": "",
                }))
                .into_response()
            }
            Err(error) => server_error("file auth", error),
        }
    }
}

/// Receive the raw attachment bytes (prefix/suffix are empty) and store them.
async fn upload_put(Path(upload_key): Path<String>, body: Bytes) -> Response {
    if let Err(error) = tokio::fs::create_dir_all(&cfg().storage_dir).await {
        tracing::error!(%error, "create storage dir");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match tokio::fs::write(file_path(&upload_key), &body).await {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(error) => {
            tracing::error!(%error, "store file");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The client reads md5/mtime from this response's headers (it does not follow
/// the redirect automatically) and then downloads the bytes from `Location`.
async fn file_get(Path((_id, key)): Path<(String, String)>) -> Response {
    let (md5, _filename, mtime) = match store::file_meta(pool(), &key).await {
        Ok(Some(meta)) => meta,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return server_error("file meta", error),
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        "location",
        format!("{}/files/{}", cfg().public_url, key)
            .parse()
            .unwrap(),
    );
    headers.insert(
        "zotero-file-modification-time",
        mtime.to_string().parse().unwrap(),
    );
    headers.insert("zotero-file-md5", md5.parse().unwrap());
    headers.insert("zotero-file-compressed", "No".parse().unwrap());
    (StatusCode::FOUND, headers).into_response()
}

/// Serve the raw attachment bytes the file_get redirect points at.
async fn file_download(Path(key): Path<String>) -> Response {
    match tokio::fs::read(file_path(&key)).await {
        Ok(bytes) => bytes.into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// --- middleware -------------------------------------------------------------

/// Decode gzip write bodies, log the request, and reject anything without the
/// configured key except the bootstrap (key/session creation, login) endpoints.
async fn log_and_auth(req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let raw = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_else(|_| Bytes::new());

    let gzipped = parts
        .headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|e| e.contains("gzip"));
    let bytes = if gzipped {
        use std::io::Read;
        let mut decoder = flate2::read::GzDecoder::new(&raw[..]);
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
    let is_bootstrap = (parts.method == axum::http::Method::POST && path == "/keys")
        || path.starts_with("/keys/sessions")
        || path.starts_with("/uploads")
        || path.starts_with("/files")
        || path == "/login";
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
            .post(move |headers: HeaderMap, body: Bytes| write(kind, headers, body))
            .patch(move |headers: HeaderMap, body: Bytes| write(kind, headers, body))
            .delete(
                move |headers: HeaderMap, Query(p): Query<HashMap<String, String>>| {
                    delete(kind, headers, p)
                },
            )
    };

    Router::new()
        .route("/keys", post(create_key))
        .route("/keys/current", get(key_current))
        .route("/keys/sessions", post(create_session))
        .route(
            "/keys/sessions/{token}",
            get(check_session).delete(cancel_session),
        )
        .route("/login", get(login_page))
        .route("/users/{id}/groups", get(groups))
        .route(
            "/users/{id}/settings",
            get(settings_read)
                .post(settings_write)
                .delete(settings_write),
        )
        .route("/users/{id}/collections", objects("collection"))
        // CLI listing of a collection's items (query-only; no sync use).
        .route("/users/{id}/collections/{key}/items", get(collection_items))
        .route("/users/{id}/searches", objects("search"))
        // Items share the write/delete logic but take a dedicated GET that adds
        // the CLI query API alongside the two sync reads.
        .route(
            "/users/{id}/items",
            get(items_get)
                .post(move |headers: HeaderMap, body: Bytes| write("item", headers, body))
                .patch(move |headers: HeaderMap, body: Bytes| write("item", headers, body))
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
        .route("/files/{key}", get(file_download))
        .route("/users/{id}/deleted", get(deleted))
        // Attachment uploads exceed the default 2 MiB extractor limit; the
        // middleware already buffers the whole body, so lift it.
        .layer(DefaultBodyLimit::disable())
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
        storage_dir: std::env::var("ZHOST_STORAGE_DIR")
            .unwrap_or_else(|_| "/tmp/zhost-storage".into()),
    });

    let pool = store::connect(&cfg().database_url)
        .await
        .expect("connect to database");
    let _ = POOL.set(pool);

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
