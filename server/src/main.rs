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
    extract::{DefaultBodyLimit, Query, State},
    http::HeaderMap,
    middleware,
    routing::{get, post},
    Router,
};
use sqlx::PgPool;

use crate::config::Config;

use crate::handlers::files::{
    cleanup_pending_uploads, file_get, file_post, pending_upload_cleanup_loop, upload_put,
};
use crate::handlers::fulltext::{fulltext_item, fulltext_versions, fulltext_write};
use crate::handlers::groups::{group_get, groups};
use crate::handlers::keys::key_current;
use crate::handlers::login::{
    cancel_session, check_session, create_session, login_authorize, login_page,
};
use crate::handlers::objects::{
    collection_items, collection_items_top, delete, items_get, items_top, items_trash, read, write,
};
use crate::handlers::settings::{deleted, settings_delete, settings_read, settings_write};
use crate::handlers::tags::{tags_delete, tags_get};
use crate::http::middleware::{log_and_auth, MAX_BODY};

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    pool: PgPool,
    storage: Arc<s3::Storage>,
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
