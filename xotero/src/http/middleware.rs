use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};

use crate::domain::{LibraryAccess, RequestContext};
use crate::error::server_error;
use crate::http::access::request_access_scope;
use crate::http::validation::{
    is_group_discovery_path, path_group_data_id, path_user_id, request_log_path,
};
use crate::{store, AppState};

/// Maximum buffered request body. Attachments can be large; everything else is
/// tiny. A finite cap bounds per-request memory so one device can't OOM the host
/// (the body is fully buffered by the auth middleware before handlers run).
pub(crate) const MAX_BODY: usize = 256 * 1024 * 1024;

/// Resolve static recovery keys and database-owned keys against live identity
/// state. Database errors remain errors so an outage cannot masquerade as a bad
/// credential.
async fn authenticate_request(
    state: &AppState,
    headers: &HeaderMap,
) -> sqlx::Result<Option<RequestContext>> {
    let Some(token) = headers
        .get("zotero-api-key")
        .and_then(|v| v.to_str().ok())
        .filter(|token| !token.is_empty())
    else {
        return Ok(None);
    };

    if let Some(permissions) = state.config.keys.get(token).copied() {
        return Ok(
            store::bootstrap_principal(&state.pool, state.config.user_id)
                .await?
                .map(|principal| RequestContext {
                    presented_key: token.to_owned(),
                    principal,
                    permissions,
                    api_key_id: None,
                }),
        );
    }

    let digest = Sha256::digest(token.as_bytes());
    Ok(store::authenticate_api_key(&state.pool, digest.as_slice())
        .await?
        .map(|(api_key_id, principal, permissions)| RequestContext {
            presented_key: token.to_owned(),
            principal,
            permissions,
            api_key_id: Some(api_key_id),
        }))
}

/// Decode gzip write bodies, log safe routing metadata, and reject anything
/// without the configured key except bootstrap endpoints. Query strings, form
/// values, and content never enter logs because they can hold capability tokens
/// and private library data.
pub(crate) async fn log_and_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
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
    let path = parts.uri.path().to_owned();
    let log_path = request_log_path(&path).to_owned();
    tracing::info!(
        %method,
        path = %log_path,
        api_version = %header("zotero-api-version"),
        if_unmod = %header("if-unmodified-since-version"),
        body_bytes = bytes.len(),
        "request"
    );

    let mut selected_access = None;
    let is_bootstrap =
        path.starts_with("/keys/sessions") || path.starts_with("/uploads") || path == "/login";
    if !is_bootstrap {
        let context = match authenticate_request(&state, &parts.headers).await {
            Ok(Some(context)) => context,
            Ok(None) => return (StatusCode::FORBIDDEN, "invalid API key").into_response(),
            Err(error) => return server_error("API key authentication", error),
        };
        let is_group_discovery = is_group_discovery_path(&path);
        match path_user_id(&path) {
            Ok(Some(path_user_id)) if path_user_id == context.principal.user_id.get() => {}
            Ok(Some(_)) if is_group_discovery => {}
            Ok(Some(_)) => return (StatusCode::FORBIDDEN, "user access denied").into_response(),
            Err(()) => return StatusCode::NOT_FOUND.into_response(),
            Ok(None) => {}
        }
        let group_id = match path_group_data_id(&path) {
            Ok(group_id) => group_id,
            Err(()) => return StatusCode::NOT_FOUND.into_response(),
        };
        let access = if let Some(group_id) = group_id {
            match store::resolve_group_library(
                &state.pool,
                group_id,
                context.principal.user_id,
                context.api_key_id,
                context.api_key_id.is_none().then_some(context.permissions),
            )
            .await
            {
                Ok(store::GroupLibraryResolution::Allowed(access)) => Some(access),
                Ok(store::GroupLibraryResolution::Denied) => {
                    return (StatusCode::FORBIDDEN, "group access denied").into_response();
                }
                Ok(store::GroupLibraryResolution::Missing) => {
                    return StatusCode::NOT_FOUND.into_response();
                }
                Err(error) => return server_error("resolve group library", error),
            }
        } else if path != "/keys/current" && !is_group_discovery {
            if !context.permissions.library {
                return (StatusCode::FORBIDDEN, "library access denied").into_response();
            }
            Some(LibraryAccess {
                library_id: context.principal.library_id,
                group_id: None,
                permissions: context.permissions,
                is_admin: true,
                file_write: context.permissions.write && context.permissions.files,
            })
        } else {
            None
        };
        if path.contains("/file") && access.is_some_and(|access| !access.permissions.files) {
            return (StatusCode::FORBIDDEN, "file access denied").into_response();
        }
        let mutating = matches!(
            parts.method,
            axum::http::Method::POST
                | axum::http::Method::PUT
                | axum::http::Method::PATCH
                | axum::http::Method::DELETE
        );
        if mutating && path.contains("/file") && access.is_some_and(|access| !access.file_write) {
            return (StatusCode::FORBIDDEN, "file editing denied").into_response();
        }
        if mutating && access.is_some_and(|access| !access.permissions.write) {
            return (StatusCode::FORBIDDEN, "read-only API key").into_response();
        }
        selected_access = access;
        parts.extensions.insert(context);
    }

    let request = Request::from_parts(parts, Body::from(bytes));
    let response = match selected_access {
        Some(access) => request_access_scope(access, next.run(request)).await,
        None => next.run(request).await,
    };
    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        tracing::warn!(
            %method,
            path = %log_path,
            status = status.as_u16(),
            "response error"
        );
    }
    response
}
