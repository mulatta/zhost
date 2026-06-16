//! Self-hosted Zotero Web API v3 sync server.
//!
//! Objects are stored as opaque jsonb blobs in PostgreSQL (see `store`); each
//! write bumps a single library version counter so the client's `since` reads
//! and `If-Unmodified-Since-Version` writes stay coherent. See SPEC.md for the
//! protocol contract.

mod store;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};

use axum::{
    body::{Body, Bytes},
    extract::{Form, Path, Query, Request},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use sqlx::PgPool;

struct Config {
    key: String,
    user_id: u64,
    bind: String,
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

/// Full access for the single configured user; no groups.
fn access() -> Value {
    json!({
        "user": { "library": true, "files": true, "notes": true, "write": true },
        "groups": {}
    })
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
            "key": cfg().key,
            "userID": cfg().user_id,
            "username": "zhost",
            "displayName": "zhost",
            "access": access(),
        })),
    )
        .into_response()
}

async fn key_current() -> Response {
    Json(json!({
        "userID": cfg().user_id,
        "username": "zhost",
        "displayName": "zhost",
        "access": access(),
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
            "loginURL": format!("http://{}/login", cfg().bind),
        })),
    )
        .into_response()
}

async fn check_session() -> Response {
    Json(json!({
        "status": "completed",
        "apiKey": cfg().key,
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

async fn write(kind: &str, body: Bytes) -> Response {
    let batch: Vec<Value> = match serde_json::from_slice(&body) {
        Ok(batch) => batch,
        Err(error) => {
            tracing::warn!(%error, "malformed write body");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    match store::write(pool(), kind, batch).await {
        Ok((version, successful)) => (
            version_headers(version),
            Json(json!({
                "successful": successful,
                "success": {},
                "unchanged": {},
                "failed": {},
            })),
        )
            .into_response(),
        Err(error) => server_error("write", error),
    }
}

async fn delete(kind: &str, params: HashMap<String, String>) -> Response {
    let keys = params
        .get(&format!("{kind}Key"))
        .map(|csv| csv.split(',').map(String::from).collect::<Vec<_>>())
        .unwrap_or_default();
    match store::delete(pool(), kind, &keys).await {
        Ok(version) => (StatusCode::NO_CONTENT, version_headers(version)).into_response(),
        Err(error) => server_error("delete", error),
    }
}

async fn settings_read() -> Response {
    match store::settings(pool()).await {
        Ok(value) => (current_headers().await, Json(value)).into_response(),
        Err(error) => server_error("settings", error),
    }
}

async fn settings_write(body: Bytes) -> Response {
    let value: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    match store::write_settings(pool(), value).await {
        Ok(version) => (StatusCode::NO_CONTENT, version_headers(version)).into_response(),
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

/// Acknowledge full-text content without storing it yet (storage arrives in a
/// later slice); the client just needs each submitted item marked successful.
async fn fulltext_write(body: Bytes) -> Response {
    let items: Vec<Value> = serde_json::from_slice(&body).unwrap_or_default();
    let version = store::current_version(pool()).await.unwrap_or(0);
    let mut successful = serde_json::Map::new();
    for (index, item) in items.iter().enumerate() {
        let key = item.get("key").and_then(Value::as_str).unwrap_or_default();
        successful.insert(index.to_string(), json!({ "key": key, "version": version }));
    }
    (
        version_headers(version),
        Json(json!({ "successful": successful, "unchanged": {}, "failed": {} })),
    )
        .into_response()
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
                    "url": format!("http://{}/uploads/{}", cfg().bind, key),
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

/// Serve attachment bytes with the md5/mtime the client verifies against.
async fn file_get(Path((_id, key)): Path<(String, String)>) -> Response {
    let (md5, _filename, mtime) = match store::file_meta(pool(), &key).await {
        Ok(Some(meta)) => meta,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return server_error("file meta", error),
    };
    match tokio::fs::read(file_path(&key)).await {
        Ok(bytes) => {
            let mut headers = HeaderMap::new();
            headers.insert("etag", md5.parse().unwrap());
            headers.insert(
                "zotero-file-modification-time",
                mtime.to_string().parse().unwrap(),
            );
            (headers, bytes).into_response()
        }
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
        || path == "/login";
    if !is_bootstrap {
        let authorised = parts
            .headers
            .get("zotero-api-key")
            .and_then(|v| v.to_str().ok())
            == Some(cfg().key.as_str());
        if !authorised {
            return (StatusCode::FORBIDDEN, "invalid API key").into_response();
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
            .post(move |body: Bytes| write(kind, body))
            .patch(move |body: Bytes| write(kind, body))
            .delete(move |Query(p): Query<HashMap<String, String>>| delete(kind, p))
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
        .route("/users/{id}/searches", objects("search"))
        .route("/users/{id}/items", objects("item"))
        .route(
            "/users/{id}/items/top",
            get(|Query(p): Query<HashMap<String, String>>| read("item", p)),
        )
        .route("/users/{id}/fulltext", get(groups).post(fulltext_write))
        .route(
            "/users/{id}/items/{key}/file",
            get(file_get).post(file_post),
        )
        .route("/uploads/{key}", post(upload_put))
        .route("/users/{id}/deleted", get(deleted))
        .layer(middleware::from_fn(log_and_auth))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let _ = CFG.set(Config {
        key: std::env::var("ZHOST_API_KEY").unwrap_or_else(|_| "zhost-dev-key".into()),
        user_id: std::env::var("ZHOST_USER_ID")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1),
        bind: std::env::var("ZHOST_BIND").unwrap_or_else(|_| "127.0.0.1:8189".into()),
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
