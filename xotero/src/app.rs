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
use crate::handlers::files::{file_get, file_post, upload_put};
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
use crate::s3;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) config: Arc<Config>,
    pub(crate) pool: PgPool,
    pub(crate) storage: Arc<s3::Storage>,
}

/// An unguessable upload token (128 bits of OS randomness, hex-encoded). `None`
/// if the OS RNG can't be read, so the caller can fail the request rather than
/// panic.
pub(crate) fn router(state: AppState) -> Router {
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
