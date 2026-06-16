//! Minimal Zotero Web API v3 server that answers an empty-library sync.
//!
//! This stub holds no state: every library listing is empty and the library
//! version is fixed at zero. Its purpose is to prove that a URL-redirected
//! stock Zotero client completes a no-op sync against us, and to log the exact
//! requests it sends so later, stateful work can be tested against real client
//! behaviour rather than assumptions. See SPEC.md for the full contract.

use std::sync::OnceLock;

use axum::{
    body::{Body, Bytes},
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

struct Config {
    key: String,
    user_id: u64,
    bind: String,
}

static CFG: OnceLock<Config> = OnceLock::new();

fn cfg() -> &'static Config {
    CFG.get().expect("config initialised in main")
}

/// Full access for the single configured user; no groups.
fn access() -> Value {
    json!({
        "user": { "library": true, "files": true, "notes": true, "write": true },
        "groups": {}
    })
}

/// Attach the library-version header every data response must carry. The empty
/// library is always at version zero.
fn versioned(value: Value) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert("last-modified-version", HeaderValue::from_static("0"));
    (headers, Json(value)).into_response()
}

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

/// `format=versions` listings: a key→version map, empty for an empty library.
async fn empty_map() -> Response {
    versioned(json!({}))
}

async fn deleted() -> Response {
    versioned(json!({ "collections": [], "searches": [], "items": [], "tags": [] }))
}

/// Log every request (and any body) so a live sync can be captured as fixtures,
/// and reject anything without the configured key except the key-creation call.
async fn log_and_auth(req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_else(|_| Bytes::new());

    let api_version = parts
        .headers
        .get("zotero-api-version")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    tracing::info!(
        method = %parts.method,
        uri = %parts.uri,
        api_version,
        body = %String::from_utf8_lossy(&bytes),
        "request"
    );

    let is_create_key = parts.method == axum::http::Method::POST && parts.uri.path() == "/keys";
    if !is_create_key {
        let authorised = parts
            .headers
            .get("zotero-api-key")
            .and_then(|v| v.to_str().ok())
            == Some(cfg().key.as_str());
        if !authorised {
            return (StatusCode::FORBIDDEN, "invalid API key").into_response();
        }
    }

    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

fn app() -> Router {
    Router::new()
        .route("/keys", post(create_key))
        .route("/keys/current", get(key_current))
        .route("/users/{id}/groups", get(empty_map))
        .route("/users/{id}/settings", get(empty_map))
        .route("/users/{id}/collections", get(empty_map))
        .route("/users/{id}/searches", get(empty_map))
        .route("/users/{id}/items", get(empty_map))
        .route("/users/{id}/items/top", get(empty_map))
        .route("/users/{id}/fulltext", get(empty_map))
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
    });

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
