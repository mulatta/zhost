use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::domain::{LibraryId, Permissions, RequestContext};
use crate::error::{s3_error, server_error};
use crate::http::access::{request_access, request_library};
use crate::http::headers::{conflict, current_headers, header_value, version_headers};
use crate::http::validation::valid_key;
use crate::{store, upload_token, AppState};

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

/// Attachment file endpoint. The same path serves both POST steps:
/// authorisation (`md5`/`filename`/`filesize`/`mtime` form) and registration
/// (`upload` form, after the bytes have been PUT to the upload URL).
pub(crate) async fn file_post(
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

pub(crate) async fn cleanup_pending_uploads(state: &AppState) {
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

pub(crate) async fn pending_upload_cleanup_loop(state: AppState) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        cleanup_pending_uploads(&state).await;
    }
}

/// Receive the raw attachment bytes for a pending upload token, verify them
/// against the authorized md5/filesize, and store an immutable candidate.
/// Rejects an unknown token. Verifying here (where the bytes are in hand) keeps
/// the integrity check server-side now that the bytes go straight to S3.
pub(crate) async fn upload_put(
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
pub(crate) async fn file_get(
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
