//! Self-hosted Zotero Web API v3 sync server.
//!
//! Objects are stored as opaque jsonb blobs in PostgreSQL (see `store`); each
//! write bumps a single library version counter so the client's `since` reads
//! and `If-Unmodified-Since-Version` writes stay coherent. See SPEC.md for the
//! protocol contract.

mod domain;
mod query;
mod s3;
mod store;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Form, OriginalUri, Path, Query, RawQuery, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use hmac::{Hmac, Mac};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::domain::{GroupId, LibraryAccess, LibraryId, Permissions, RequestContext, UserId};

struct Config {
    /// Static recovery token → access, loaded from secret files at boot.
    keys: HashMap<String, Permissions>,
    /// Dedicated stable secret for restart-safe browser login key derivation.
    login_kdf_key: Vec<u8>,
    user_id: UserId,
    username: String,
    display_name: String,
    /// Migration 0001 creates one legacy personal library with ID 1. Keeping its
    /// scope in application state makes every storage call explicit.
    library_id: LibraryId,
    bind: String,
    /// Client-facing base URL (e.g. the reverse-proxy address). Used for the
    /// login and upload URLs handed to the client, which must be reachable by it
    /// — not the internal bind address. (Downloads use a pre-signed bucket URL.)
    public_url: String,
    database_url: String,
    s3: s3::Config,
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    pool: PgPool,
    storage: Arc<s3::Storage>,
}

tokio::task_local! {
    static REQUEST_ACCESS: LibraryAccess;
}

fn request_library() -> LibraryId {
    REQUEST_ACCESS
        .try_with(|access| access.library_id)
        .expect("library store access requires authenticated request context")
}

fn request_access() -> LibraryAccess {
    REQUEST_ACCESS
        .try_with(|access| *access)
        .expect("library access requires authenticated request context")
}

fn request_permissions() -> Permissions {
    REQUEST_ACCESS
        .try_with(|access| access.permissions)
        .expect("permission-aware access requires authenticated request context")
}

fn upload_storage_key(library_id: LibraryId, upload_token: &str) -> String {
    format!("libraries/{}/uploads/{upload_token}", library_id.get())
}

/// In-flight file uploads, keyed by an unguessable upload token (not the item
/// key, which is guessable) and remembered between the authorisation, upload and
/// registration steps. Pruned on insert so a never-completed upload can't leak.
static PENDING: LazyLock<Mutex<HashMap<String, PendingUpload>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// How long an authorized-but-unfinished upload stays valid.
const PENDING_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

#[derive(Clone)]
struct PendingUpload {
    /// Library that authorized the upload token.
    library_id: LibraryId,
    group_id: Option<GroupId>,
    /// DB key digest, when a user-owned key authorized this upload. Static
    /// recovery keys have no digest and are checked through bootstrap identity.
    api_key_hash: Option<Vec<u8>>,
    bootstrap_user_id: Option<UserId>,
    bootstrap_permissions: Option<Permissions>,
    /// The attachment item the candidate bytes belong to.
    item_key: String,
    /// MD5 that was current when this upload was authorized. `None` means the
    /// authorization required no registered file to exist.
    expected_md5: Option<String>,
    /// Immutable candidate object. Registration atomically makes this live by
    /// storing the pointer beside the file metadata.
    blob_key: String,
    md5: String,
    filename: String,
    filesize: i64,
    mtime: i64,
    state: PendingUploadState,
    created: std::time::Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingUploadState {
    Authorized,
    Uploading,
    Uploaded,
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

/// A Zotero object key: exactly 8 chars from a base32 alphabet (digits 2-9 and
/// A-Z without the ambiguous `0`/`1`/`O`). The client rejects anything else
/// ("key is not valid") and queues it, so reject a malformed key at the API
/// boundary rather than let it sync and break the client. Object keys only —
/// settings keys are arbitrary names handled on a separate path.
fn valid_object_key(key: &str) -> bool {
    key.len() == 8
        && key
            .bytes()
            .all(|b| b"23456789ABCDEFGHIJKLMNPQRSTUVWXYZ".contains(&b))
}

/// Zotero omits false permission fields and empty access families.
fn access_payload(permissions: Permissions, group_grants: &[store::GroupGrant]) -> Value {
    let mut user = Map::new();
    for (name, allowed) in [
        ("library", permissions.library),
        ("files", permissions.files),
        ("notes", permissions.notes),
        ("write", permissions.write),
    ] {
        if allowed {
            user.insert(name.into(), Value::Bool(true));
        }
    }
    let mut access = Map::new();
    if !user.is_empty() {
        access.insert("user".into(), Value::Object(user));
    }
    let mut groups = Map::new();
    for grant in group_grants.iter().filter(|grant| grant.library) {
        groups.insert(
            grant
                .group_id
                .map_or_else(|| "all".to_string(), |id| id.get().to_string()),
            json!({ "library": true, "write": grant.write }),
        );
    }
    if !groups.is_empty() {
        access.insert("groups".into(), Value::Object(groups));
    }
    Value::Object(access)
}

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

fn version_headers(version: i64) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "last-modified-version",
        version.to_string().parse().unwrap(),
    );
    headers
}

async fn current_headers(state: &AppState) -> HeaderMap {
    version_headers(
        store::current_version(&state.pool, request_library())
            .await
            .unwrap_or(0),
    )
}

/// For a since/versions read: the current library version, and whether the
/// client already holds everything up to it (→ `304 Not Modified`; nothing has a
/// version greater than `since`). `since == 0` is the initial pull, so never
/// 304 it. One DB read, so the caller reuses `current` for the response's
/// `Last-Modified-Version` instead of querying it again.
async fn since_check(state: &AppState, since: i64) -> (i64, bool) {
    let current = store::current_version(&state.pool, request_library())
        .await
        .unwrap_or(0);
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

async fn key_current(
    State(state): State<AppState>,
    Extension(context): Extension<RequestContext>,
) -> Response {
    let group_grants = match context.api_key_id {
        Some(api_key_id) => match store::api_key_group_grants(&state.pool, api_key_id).await {
            Ok(grants) => grants,
            Err(error) => return server_error("read API key group grants", error),
        },
        None if context.permissions.library => vec![store::GroupGrant {
            group_id: None,
            library: true,
            write: context.permissions.write,
        }],
        None => Vec::new(),
    };
    Json(json!({
        "key": context.presented_key,
        "userID": context.principal.user_id.get(),
        "username": context.principal.username,
        "displayName": context.principal.display_name,
        "access": access_payload(context.permissions, &group_grants),
    }))
    .into_response()
}

/// Zotero's "Login" uses a browser-authorised session rather than credentials:
/// the client opens `loginURL` in the user's browser, then polls the session
/// until it reports `status: "completed"` with a key. Mint a pending session and
/// point `loginURL` at our `/login` (which the user must pass an SSO gate to
/// reach); the key is withheld until that authorises the session.
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

async fn create_session(State(state): State<AppState>, headers: HeaderMap) -> Response {
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
async fn check_session(State(state): State<AppState>, Path(token): Path<String>) -> Response {
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

async fn cancel_session(State(state): State<AppState>, Path(token): Path<String>) -> Response {
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
async fn login_page(
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
async fn login_authorize(
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

// --- library data -----------------------------------------------------------

fn group_json(group: store::GroupMetadata, public_url: &str) -> Value {
    let mut data = Map::new();
    data.insert("id".into(), Value::from(group.id.get()));
    data.insert("version".into(), Value::from(group.version));
    data.insert("name".into(), Value::from(group.name));
    data.insert("owner".into(), Value::from(group.owner));
    data.insert("type".into(), Value::from(group.group_type));
    data.insert("description".into(), Value::from(group.description));
    data.insert("url".into(), Value::from(group.url));
    data.insert("libraryEditing".into(), Value::from(group.library_editing));
    data.insert("libraryReading".into(), Value::from(group.library_reading));
    data.insert("fileEditing".into(), Value::from(group.file_editing));
    if !group.admins.is_empty() {
        data.insert("admins".into(), json!(group.admins));
    }
    if !group.members.is_empty() {
        data.insert("members".into(), json!(group.members));
    }
    json!({
        "id": group.id.get(),
        "version": group.version,
        "links": {
            "self": {
                "href": format!("{public_url}/groups/{}", group.id.get()),
                "type": "application/json"
            },
            "alternate": {
                "href": format!("https://www.zotero.org/groups/{}", group.id.get()),
                "type": "text/html"
            }
        },
        "meta": {
            "created": group.created,
            "lastModified": group.last_modified,
            "numItems": group.num_items,
            "isAdmin": matches!(group.role.as_str(), "owner" | "admin")
        },
        "data": Value::Object(data),
    })
}

async fn groups(
    State(state): State<AppState>,
    Extension(context): Extension<RequestContext>,
    Path(target_user_id): Path<i64>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(target_user_id) = UserId::new(target_user_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match store::active_user_exists(&state.pool, target_user_id).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return server_error("read group-list user", error),
    }
    let groups = match store::groups_for_user(
        &state.pool,
        target_user_id,
        context.principal.user_id,
        context.api_key_id,
        context.api_key_id.is_none() && context.permissions.library,
    )
    .await
    {
        Ok(groups) => groups,
        Err(error) => return server_error("list groups", error),
    };
    let mut headers = HeaderMap::new();
    headers.insert("total-results", header_value(&groups.len().to_string()));
    headers.insert(
        "link",
        header_value(&format!(
            "<{}/users/{}/groups>; rel=\"alternate\"",
            state.config.public_url,
            target_user_id.get()
        )),
    );
    if params
        .get("format")
        .is_some_and(|format| format == "versions")
    {
        let versions = groups
            .into_iter()
            .map(|group| (group.id.get().to_string(), Value::from(group.version)))
            .collect();
        (headers, Json(Value::Object(versions))).into_response()
    } else {
        let values: Vec<_> = groups
            .into_iter()
            .map(|group| group_json(group, &state.config.public_url))
            .collect();
        (headers, Json(values)).into_response()
    }
}

async fn group_get(
    State(state): State<AppState>,
    Extension(context): Extension<RequestContext>,
    Path(group_id): Path<i64>,
) -> Response {
    let Some(group_id) = GroupId::new(group_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match store::group_for_user(
        &state.pool,
        group_id,
        context.principal.user_id,
        context.api_key_id,
        context.api_key_id.is_none() && context.permissions.library,
    )
    .await
    {
        Ok(Some(group)) => (
            version_headers(group.version),
            Json(group_json(group, &state.config.public_url)),
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => server_error("read group", error),
    }
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

/// The `Link: <…>; rel="next"` header for the page after `start`, preserving the
/// request's other params and pointing at the public (reverse-proxy) URL.
fn next_link(config: &Config, path: &str, raw: Option<&str>, start: i64) -> String {
    let mut pairs: Vec<(String, String)> = raw
        .and_then(|q| serde_urlencoded::from_str(q).ok())
        .unwrap_or_default();
    pairs.retain(|(k, _)| k != "start");
    pairs.push(("start".into(), start.to_string()));
    let qs = serde_urlencoded::to_string(&pairs).unwrap_or_default();
    format!("<{}{}?{}>; rel=\"next\"", config.public_url, path, qs)
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
        let pending = {
            let mut pending = PENDING.lock().unwrap();
            if pending
                .get(token)
                .is_some_and(|upload| upload.created.elapsed() >= PENDING_TTL)
            {
                pending.remove(token);
            }
            pending.get(token).cloned()
        };
        let Some(upload) = pending else {
            return (StatusCode::BAD_REQUEST, "no pending upload").into_response();
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
        if upload.state != PendingUploadState::Uploaded {
            return (StatusCode::BAD_REQUEST, "no uploaded bytes").into_response();
        }
        return match store::register_file(
            &state.pool,
            request_library(),
            &key,
            store::FileRegistration {
                expected_md5: upload.expected_md5.as_deref(),
                blob_key: &upload.blob_key,
                md5: &upload.md5,
                filename: &upload.filename,
                filesize: upload.filesize,
                mtime: upload.mtime,
            },
        )
        .await
        {
            Ok(store::FileRegistrationOutcome::Done(version)) => {
                PENDING.lock().unwrap().remove(token);
                (StatusCode::NO_CONTENT, version_headers(version)).into_response()
            }
            Ok(store::FileRegistrationOutcome::Conflict(current)) => {
                PENDING.lock().unwrap().remove(token);
                conflict(current)
            }
            Ok(store::FileRegistrationOutcome::InvalidItem) => {
                PENDING.lock().unwrap().remove(token);
                (StatusCode::CONFLICT, "attachment item changed").into_response()
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
        Ok(meta) => meta.map(|(m, _, _)| m),
        Err(error) => return server_error("file auth", error),
    };

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

    // Authorize: mint an unguessable token, remember the upload (pruning stale
    // ones), and hand back the upload URL. Bytes land at a token-specific path.
    let Some(token) = upload_token() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    {
        let mut pending = PENDING.lock().unwrap();
        pending.retain(|_, u| u.created.elapsed() < PENDING_TTL);
        pending.insert(
            token.clone(),
            PendingUpload {
                library_id: request_library(),
                group_id: request_access().group_id,
                api_key_hash: if state.config.keys.contains_key(&context.presented_key) {
                    None
                } else {
                    Some(Sha256::digest(context.presented_key.as_bytes()).to_vec())
                },
                bootstrap_user_id: state
                    .config
                    .keys
                    .contains_key(&context.presented_key)
                    .then_some(context.principal.user_id),
                bootstrap_permissions: state
                    .config
                    .keys
                    .contains_key(&context.presented_key)
                    .then_some(context.permissions),
                item_key: key.clone(),
                expected_md5: if if_none_match {
                    None
                } else {
                    stored_md5.clone()
                },
                blob_key: upload_storage_key(request_library(), &token),
                md5,
                filename: form.get("filename").cloned().unwrap_or_default(),
                filesize: form
                    .get("filesize")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                mtime: form.get("mtime").and_then(|s| s.parse().ok()).unwrap_or(0),
                state: PendingUploadState::Authorized,
                created: std::time::Instant::now(),
            },
        );
    }
    // Empty prefix/suffix: the client PUTs the raw file bytes to url.
    Json(json!({
        "url": format!("{}/uploads/{}", state.config.public_url, token),
        "uploadKey": token,
        "contentType": "application/octet-stream",
        "prefix": "",
        "suffix": "",
    }))
    .into_response()
}

async fn upload_scope_authorized(
    state: &AppState,
    upload: &PendingUpload,
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

/// Receive the raw attachment bytes for a pending upload token, verify them
/// against the authorized md5/filesize, and store an immutable candidate.
/// Rejects an unknown token. Verifying here (where the bytes are in hand) keeps
/// the integrity check server-side now that the bytes go straight to S3.
async fn upload_put(
    State(state): State<AppState>,
    Path(token): Path<String>,
    body: Bytes,
) -> Response {
    let pending = {
        let mut pending = PENDING.lock().unwrap();
        if pending
            .get(&token)
            .is_some_and(|upload| upload.created.elapsed() >= PENDING_TTL)
        {
            pending.remove(&token);
        }
        pending.get(&token).cloned()
    };
    let Some(upload) = pending else {
        return (StatusCode::BAD_REQUEST, "unknown upload token").into_response();
    };
    if upload.state != PendingUploadState::Authorized {
        return (StatusCode::CONFLICT, "upload token already used").into_response();
    }
    if let Some(token_hash) = &upload.api_key_hash {
        let authenticated = match store::authenticate_api_key(&state.pool, token_hash).await {
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
        let principal = match store::bootstrap_principal(&state.pool, user_id).await {
            Ok(Some(principal)) => principal,
            Ok(None) => return (StatusCode::FORBIDDEN, "bootstrap user disabled").into_response(),
            Err(error) => return server_error("upload authorization", error),
        };
        let Some(permissions) = upload.bootstrap_permissions else {
            return (StatusCode::FORBIDDEN, "bootstrap permission missing").into_response();
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
    if body.len() as i64 != upload.filesize || actual_md5 != upload.md5.to_lowercase() {
        tracing::warn!("uploaded bytes do not match authorization");
        return (
            StatusCode::BAD_REQUEST,
            "uploaded bytes do not match md5/filesize",
        )
            .into_response();
    }
    {
        let mut pending = PENDING.lock().unwrap();
        let Some(current) = pending.get_mut(&token) else {
            return (StatusCode::BAD_REQUEST, "unknown upload token").into_response();
        };
        if current.created.elapsed() >= PENDING_TTL {
            pending.remove(&token);
            return (StatusCode::BAD_REQUEST, "expired upload token").into_response();
        }
        if current.state != PendingUploadState::Authorized {
            return (StatusCode::CONFLICT, "upload token already used").into_response();
        }
        current.state = PendingUploadState::Uploading;
    }
    if let Err(error) = state
        .storage
        .put(&upload.blob_key, &body, "application/octet-stream")
        .await
    {
        if let Some(current) = PENDING.lock().unwrap().get_mut(&token) {
            if current.state == PendingUploadState::Uploading {
                current.state = PendingUploadState::Authorized;
            }
        }
        return s3_error("store file", error);
    }
    // Mark the pending upload stored so registration can commit its metadata.
    if let Some(u) = PENDING.lock().unwrap().get_mut(&token) {
        if u.state == PendingUploadState::Uploading {
            u.state = PendingUploadState::Uploaded;
        }
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
    let (md5, mtime, blob_key) = match store::file_meta(&state.pool, request_library(), &key).await
    {
        Ok(Some(meta)) => meta,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return server_error("file meta", error),
    };
    let url = match state.storage.presign_get(&blob_key).await {
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

fn request_log_path(path: &str) -> &str {
    if path.starts_with("/uploads/") {
        "/uploads/{token}"
    } else if path.starts_with("/keys/sessions/") {
        "/keys/sessions/{token}"
    } else {
        path
    }
}

fn is_group_discovery_path(path: &str) -> bool {
    let segments: Vec<_> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    matches!(segments.as_slice(), ["groups", _] | ["users", _, "groups"])
}

/// Decode gzip write bodies, log safe routing metadata, and reject anything
/// without the configured key except bootstrap endpoints. Query strings, form
/// values, and content never enter logs because they can hold capability tokens
/// and private library data.
async fn log_and_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
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
        Some(access) => REQUEST_ACCESS.scope(access, next.run(request)).await,
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

fn path_user_id(path: &str) -> Result<Option<i64>, ()> {
    let mut segments = path.split('/');
    if segments.next() != Some("") || segments.next() != Some("users") {
        return Ok(None);
    }
    let id = segments.next().ok_or(())?.parse().map_err(|_| ())?;
    Ok(Some(id))
}

fn path_group_data_id(path: &str) -> Result<Option<GroupId>, ()> {
    let segments: Vec<_> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.first() != Some(&"groups") || segments.len() < 3 {
        return Ok(None);
    }
    let raw = segments[1].parse().map_err(|_| ())?;
    GroupId::new(raw).map(Some).ok_or(())
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

/// Build the token→access map from secret files. `ZHOST_KEYS` is a
/// comma-separated list of `<role>:<path>` entries (`rw`/`ro`), each path a
/// single-line token (a sops-nix secret exposed via systemd LoadCredential).
/// Falls back to a single read/write key from `ZHOST_API_KEY_FILE` /
/// `ZHOST_API_KEY` for simple deployments. Prefer files over the env, which is
/// visible in /proc.
fn load_keys() -> HashMap<String, Permissions> {
    let read_token = |path: &str| {
        std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read key file {path}: {e}"))
            .trim()
            .to_string()
    };
    let mut keys = HashMap::new();
    if let Ok(manifest) = std::env::var("ZHOST_KEYS") {
        for entry in manifest.split(',').filter(|s| !s.is_empty()) {
            let (role, path) = entry
                .split_once(':')
                .unwrap_or_else(|| panic!("ZHOST_KEYS entry not <role>:<path>: {entry}"));
            let write = role == "rw";
            let token = read_token(path);
            keys.insert(token, Permissions::recovery(write));
        }
    } else {
        let token = match std::env::var("ZHOST_API_KEY_FILE") {
            Ok(path) => read_token(&path),
            Err(_) => std::env::var("ZHOST_API_KEY").unwrap_or_else(|_| "zhost-dev-key".into()),
        };
        keys.insert(token, Permissions::recovery(true));
    }
    keys
}

fn load_login_kdf_key() -> Vec<u8> {
    let path = std::env::var("ZHOST_LOGIN_KDF_KEY_FILE")
        .expect("ZHOST_LOGIN_KDF_KEY_FILE must point to a stable login KDF credential");
    let mut key =
        std::fs::read(&path).unwrap_or_else(|error| panic!("read login KDF key {path}: {error}"));
    while matches!(key.last(), Some(b'\n' | b'\r')) {
        key.pop();
    }
    assert!(
        key.len() >= 32,
        "login KDF credential must contain at least 32 bytes"
    );
    key
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

    let keys = load_keys();
    let bind = std::env::var("ZHOST_BIND").unwrap_or_else(|_| "127.0.0.1:8189".into());
    let bind_address: std::net::SocketAddr =
        bind.parse().expect("ZHOST_BIND must be a socket address");
    assert!(
        bind_address.ip().is_loopback(),
        "ZHOST_BIND must be loopback so only the trusted local proxy can set OIDC headers"
    );
    let user_id = std::env::var("ZHOST_USER_ID")
        .ok()
        .and_then(|v| v.parse().ok())
        .and_then(UserId::new)
        .unwrap_or_else(|| UserId::new(1).expect("default user ID is positive"));
    let username = std::env::var("ZHOST_USERNAME").unwrap_or_else(|_| "zhost".into());
    let display_name = std::env::var("ZHOST_DISPLAY_NAME").unwrap_or_else(|_| "zhost".into());
    let library_id = LibraryId::new(1).expect("legacy library ID is positive");
    let config = Arc::new(Config {
        keys,
        login_kdf_key: load_login_kdf_key(),
        user_id,
        username: username.clone(),
        display_name: display_name.clone(),
        library_id,
        public_url: std::env::var("ZHOST_PUBLIC_URL").unwrap_or_else(|_| format!("http://{bind}")),
        bind,
        database_url: std::env::var("ZHOST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .unwrap_or_else(|_| "postgres://localhost/zhost".into()),
        s3: load_s3(),
    });

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
