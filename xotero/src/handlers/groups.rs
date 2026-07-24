use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde_json::{json, Map, Value};

use crate::domain::{GroupId, RequestContext, UserId};
use crate::error::server_error;
use crate::http::headers::{header_value, version_headers};
use crate::{store, AppState};

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

pub(crate) async fn groups(
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

pub(crate) async fn group_get(
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
