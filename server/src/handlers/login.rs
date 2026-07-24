use std::collections::HashMap;

use axum::{
    extract::{Form, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::server_error;
use crate::{store, upload_token, AppState};

fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// Derive the 24-character Zotero key returned after approval. Rejection
/// sampling avoids modulo bias because Zotero's unambiguous alphabet has 33
/// characters. Domain separation and a dedicated secret prevent a leaked,
/// expired session token from remaining an offline long-lived credential seed.
fn login_api_key(kdf_key: &[u8], session_token: &str) -> String {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHIJKLMNPQRSTUVWXYZ";
    const ACCEPT_LIMIT: u8 = 231; // 33 * 7
    let mut key = String::with_capacity(24);
    let mut counter = 0_u32;
    while key.len() < 24 {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(kdf_key).expect("HMAC accepts keys of any size");
        mac.update(b"zhost login api key\0");
        mac.update(session_token.as_bytes());
        mac.update(&counter.to_be_bytes());
        for byte in mac.finalize().into_bytes() {
            if byte < ACCEPT_LIMIT {
                key.push(ALPHABET[byte as usize % ALPHABET.len()] as char);
                if key.len() == 24 {
                    break;
                }
            }
        }
        counter += 1;
    }
    key
}

fn login_client_type(headers: &HeaderMap) -> &'static str {
    let agent = headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if agent.contains("android") {
        "android"
    } else if agent.contains("iphone") || agent.contains("ipad") || agent.contains("ios") {
        "ios"
    } else if agent.contains("windows") {
        "windows"
    } else if agent.contains("mac") {
        "mac"
    } else if agent.contains("linux") {
        "linux"
    } else {
        "unknown"
    }
}

pub(crate) async fn create_session(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(token) = upload_token() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    if let Err(error) = store::create_login_session(
        &state.pool,
        &token_hash(&token),
        login_client_type(&headers),
    )
    .await
    {
        return server_error("create login session", error);
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "sessionToken": token,
            "loginURL": format!("{}/login?session={}", state.config.public_url, token),
        })),
    )
        .into_response()
}

/// Poll a login session: hand out the key only once `/login` has authorised it,
/// otherwise report it still pending (so an unauthorised or unknown token never
/// yields a key).
pub(crate) async fn check_session(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Response {
    match store::login_session(&state.pool, &token_hash(&token)).await {
        Ok(store::LoginSession::Pending) => Json(json!({ "status": "pending" })).into_response(),
        Ok(store::LoginSession::Cancelled) => {
            Json(json!({ "status": "cancelled" })).into_response()
        }
        Ok(store::LoginSession::Completed(principal)) => Json(json!({
            "status": "completed",
            "apiKey": login_api_key(&state.config.login_kdf_key, &token),
            "userID": principal.user_id.get(),
            "username": principal.username,
        }))
        .into_response(),
        Ok(store::LoginSession::Expired) => StatusCode::GONE.into_response(),
        Ok(store::LoginSession::Missing) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => server_error("poll login session", error),
    }
}

pub(crate) async fn cancel_session(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Response {
    match store::cancel_login_session(&state.pool, &token_hash(&token)).await {
        Ok(store::LoginSessionChange::Done) => StatusCode::NO_CONTENT.into_response(),
        Ok(store::LoginSessionChange::Missing) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::CONFLICT.into_response(),
        Err(error) => server_error("cancel login session", error),
    }
}

/// The login consent page. The user reaches it from `loginURL` in their browser
/// (behind the SSO gate in production). It does **not** authorise on its own — it
/// renders a form that POSTs back to confirm. Splitting render (GET) from action
/// (POST) stops a prefetch or a cross-site `GET …/login?session=…` from silently
/// authorising a session, which would be a confused-deputy key grant: the
/// attacker creates the session (so knows its token) and only needs an
/// authenticated browser to hit the URL.
pub(crate) async fn login_page(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(token) = params.get("session") else {
        return (StatusCode::BAD_REQUEST, "missing session").into_response();
    };
    match store::login_session(&state.pool, &token_hash(token)).await {
        Ok(store::LoginSession::Pending) => {}
        Ok(store::LoginSession::Expired) => return StatusCode::GONE.into_response(),
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return server_error("read login session", error),
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
pub(crate) async fn login_authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        if origin.trim_end_matches('/') != state.config.public_url.trim_end_matches('/') {
            return (StatusCode::FORBIDDEN, "bad origin").into_response();
        }
    }
    // Trusted reverse proxy must strip caller-supplied copies and overwrite both
    // headers from verified OIDC claims. Email/user headers are never identity
    // keys: only an exact issuer+subject row can approve enrollment.
    let claim = |name| {
        let mut values = headers.get_all(name).iter();
        let value = values
            .next()?
            .to_str()
            .ok()
            .filter(|value| !value.is_empty())?;
        values.next().is_none().then_some(value)
    };
    let claims = claim("x-zhost-oidc-issuer").zip(claim("x-zhost-oidc-subject"));
    let Some((issuer, subject)) = claims else {
        return (StatusCode::FORBIDDEN, "missing verified OIDC identity").into_response();
    };
    let Some(token) = form.get("session") else {
        return (StatusCode::BAD_REQUEST, "missing session").into_response();
    };
    let api_key = login_api_key(&state.config.login_kdf_key, token);
    if state.config.keys.contains_key(&api_key) {
        return (
            StatusCode::CONFLICT,
            "generated key collides with recovery key",
        )
            .into_response();
    }
    match store::complete_login_session(
        &state.pool,
        &token_hash(token),
        &token_hash(&api_key),
        issuer,
        subject,
    )
    .await
    {
        Ok(store::LoginSessionChange::Done) => {
            (StatusCode::OK, "Authorized — return to Zotero.").into_response()
        }
        Ok(store::LoginSessionChange::Conflict) => StatusCode::CONFLICT.into_response(),
        Ok(store::LoginSessionChange::Expired) => StatusCode::GONE.into_response(),
        Ok(store::LoginSessionChange::Missing) => StatusCode::NOT_FOUND.into_response(),
        Ok(store::LoginSessionChange::UserUnavailable) => StatusCode::FORBIDDEN.into_response(),
        Err(error) => server_error("complete login session", error),
    }
}
