use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde_json::{json, Map, Value};

use crate::domain::{Permissions, RequestContext};
use crate::error::server_error;
use crate::{store, AppState};

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

pub(crate) async fn key_current(
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
