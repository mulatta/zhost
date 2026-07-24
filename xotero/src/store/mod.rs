//! PostgreSQL-backed library storage.
//!
//! Objects are stored as opaque jsonb blobs keyed by (kind, key). Every write
//! bumps the single library version counter inside a transaction and stamps the
//! affected rows with the new version, so the client's `since` reads and
//! `If-Unmodified-Since-Version` writes stay coherent.

use serde_json::{Map, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

use crate::domain::{ApiKeyId, GroupId, LibraryId, Permissions, Principal, UserId};
use crate::query::{ItemQuery, QMode};

mod fulltext;
mod groups;
mod sessions;
mod settings;
mod tags;
pub use fulltext::{fulltext_item, fulltext_versions, write_fulltext};
pub use groups::{
    active_user_exists, api_key_group_grants, group_for_user, groups_for_user,
    resolve_group_library, GroupGrant, GroupLibraryResolution, GroupMetadata,
};
pub use sessions::{
    cancel_login_session, complete_login_session, create_login_session, login_session,
    LoginSession, LoginSessionChange,
};
pub use settings::{delete_settings, settings, write_settings};
pub use tags::{delete_tags, tags};

/// `{<key_col>: version}` map from version-listing rows (the `format=versions`
/// shape shared by objects, top items and full-text).
pub(super) fn version_map(rows: Vec<PgRow>, key_col: &str) -> Value {
    let mut map = Map::new();
    for row in rows {
        map.insert(row.get(key_col), Value::from(row.get::<i64, _>("version")));
    }
    Value::Object(map)
}

/// `{key, version, data}` for one object row (data re-ordered linkMode-first).
fn object_json(row: PgRow) -> Value {
    serde_json::json!({
        "key": row.get::<String, _>("key"),
        "version": row.get::<i64, _>("version"),
        "data": order_fields(row.get::<Value, _>("data")),
    })
}

/// Zotero's `fromJSON` processes fields in object order and requires a
/// discriminator field first: an attachment needs `linkMode` before
/// `filename`/`path` ("Link mode must be set before setting attachment path"),
/// and an annotation needs `annotationType` before its other `annotation*` fields
/// ("annotationType must be set before other annotation properties"). jsonb
/// storage sorts keys alphabetically, putting both after their dependents, so
/// re-emit them first. Relies on serde_json's preserve_order feature.
fn order_fields(data: Value) -> Value {
    const FIRST: [&str; 2] = ["annotationType", "linkMode"];
    let Value::Object(map) = &data else {
        return data;
    };
    if !FIRST.iter().any(|k| map.contains_key(*k)) {
        return data;
    }
    let mut ordered = Map::new();
    for key in FIRST {
        if let Some(value) = map.get(key) {
            ordered.insert(key.to_string(), value.clone());
        }
    }
    for (key, value) in map {
        if !FIRST.contains(&key.as_str()) {
            ordered.insert(key.clone(), value.clone());
        }
    }
    Value::Object(ordered)
}

/// Generate a fresh Zotero object key (8 chars from the base32 alphabet) for an
/// object the client posted without one. 32 divides 256, so the byte→alphabet
/// mapping is unbiased.
fn generate_key() -> String {
    use std::io::Read;
    const ALPHABET: &[u8] = b"23456789ABCDEFGHIJKLMNPQRSTUVWXYZ";
    let mut buf = [0u8; 8];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf));
    buf.iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

/// A write either committed at a new version or was rejected because the
/// client's `If-Unmodified-Since-Version` no longer matches the library.
pub enum Outcome<T> {
    Done(T),
    Conflict(i64),
}

pub enum ObjectMutation<T> {
    Done(T),
    Conflict(i64),
    FileWriteDenied,
}

fn is_stored_file_attachment(value: &Value) -> bool {
    value.get("itemType").and_then(Value::as_str) == Some("attachment")
        && matches!(
            value.get("linkMode").and_then(Value::as_str),
            Some("imported_file" | "imported_url")
        )
}

/// Lock the library row, check the client's expected version, and reserve the
/// next one. Serializing on the row also prevents concurrent writes from racing.
pub(super) async fn guarded_version(
    conn: &mut sqlx::PgConnection,
    library_id: LibraryId,
    expected: Option<i64>,
) -> sqlx::Result<Outcome<i64>> {
    let current: i64 = sqlx::query("select version from library where id = $1 for update")
        .bind(library_id.get())
        .fetch_one(&mut *conn)
        .await?
        .get("version");
    if matches!(expected, Some(v) if v != current) {
        return Ok(Outcome::Conflict(current));
    }
    let version = current + 1;
    sqlx::query("update library set version = $2 where id = $1")
        .bind(library_id.get())
        .bind(version)
        .execute(&mut *conn)
        .await?;
    Ok(Outcome::Done(version))
}

pub async fn connect(url: &str) -> sqlx::Result<PgPool> {
    let pool = PgPool::connect(url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

pub(super) fn principal_from_row(row: &PgRow) -> sqlx::Result<Principal> {
    let raw_user_id = row.get::<i64, _>("user_id");
    let user_id = UserId::new(raw_user_id)
        .ok_or_else(|| sqlx::Error::Protocol(format!("invalid user ID {raw_user_id}")))?;
    let raw_library_id = row.get::<i64, _>("library_id");
    let library_id = LibraryId::new(raw_library_id)
        .ok_or_else(|| sqlx::Error::Protocol(format!("invalid library ID {raw_library_id}")))?;
    Ok(Principal {
        user_id,
        username: row.get("username"),
        display_name: row.get("display_name"),
        library_id,
    })
}

/// Bind the populated-v7 library to one configured bootstrap user.
///
/// Startup takes a transaction-scoped advisory lock and validates existing
/// rows after conflict-tolerant inserts. A changed user tuple or ownership
/// mapping therefore fails closed instead of silently rewriting identity data.
pub async fn bootstrap_identity(
    pool: &PgPool,
    user_id: UserId,
    username: &str,
    display_name: &str,
    library_id: LibraryId,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("select pg_advisory_xact_lock($1)")
        .bind(0x007a_686f_7374_i64)
        .fetch_one(&mut *tx)
        .await?;

    let library_kind: String = sqlx::query("select kind from library where id = $1 for update")
        .bind(library_id.get())
        .fetch_one(&mut *tx)
        .await?
        .get("kind");
    if library_kind != "personal" {
        return Err(sqlx::Error::Protocol(format!(
            "bootstrap library {} is {library_kind}, not personal",
            library_id.get()
        )));
    }

    sqlx::query(
        "insert into users (id, username, display_name) values ($1, $2, $3) \
         on conflict (id) do nothing",
    )
    .bind(user_id.get())
    .bind(username)
    .bind(display_name)
    .execute(&mut *tx)
    .await?;

    let user = sqlx::query("select username, display_name from users where id = $1 for update")
        .bind(user_id.get())
        .fetch_one(&mut *tx)
        .await?;
    let stored_username = user.get::<String, _>("username");
    let stored_display_name = user.get::<String, _>("display_name");
    if stored_username != username || stored_display_name != display_name {
        return Err(sqlx::Error::Protocol(format!(
            "bootstrap user {} does not match configured identity",
            user_id.get()
        )));
    }

    sqlx::query(
        "insert into personal_libraries (user_id, library_id) values ($1, $2) \
         on conflict (user_id) do nothing",
    )
    .bind(user_id.get())
    .bind(library_id.get())
    .execute(&mut *tx)
    .await?;

    let mapped_library_id: i64 =
        sqlx::query("select library_id from personal_libraries where user_id = $1 for update")
            .bind(user_id.get())
            .fetch_one(&mut *tx)
            .await?
            .get("library_id");
    if mapped_library_id != library_id.get() {
        return Err(sqlx::Error::Protocol(format!(
            "bootstrap user {} already owns library {mapped_library_id}",
            user_id.get()
        )));
    }

    // Explicit bootstrap IDs do not advance an identity sequence. Never move a
    // sequence backwards if higher generated IDs were later deleted.
    sqlx::query(
        "select setval( \
             pg_get_serial_sequence('users', 'id'), \
             greatest( \
                 (select coalesce(max(id), 1) from users), \
                 (select last_value from users_id_seq) \
             ), \
             true \
         )",
    )
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await
}

/// Add one externally authenticated identity to the configured bootstrap user.
/// Existing tuple ownership is never rewritten. A new configured tuple adds a
/// mapping; removing an old provider identity remains an explicit DB operation.
pub async fn bootstrap_external_identity(
    pool: &PgPool,
    issuer: &str,
    subject: &str,
    user_id: UserId,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "insert into external_identities (issuer, subject, user_id) \
         values ($1, $2, $3) on conflict (issuer, subject) do nothing",
    )
    .bind(issuer)
    .bind(subject)
    .bind(user_id.get())
    .execute(&mut *tx)
    .await?;
    let mapped_user_id: i64 = sqlx::query_scalar(
        "select user_id from external_identities where issuer = $1 and subject = $2 for update",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_one(&mut *tx)
    .await?;
    if mapped_user_id != user_id.get() {
        return Err(sqlx::Error::Protocol(format!(
            "external identity ({issuer}, {subject}) already belongs to user {mapped_user_id}"
        )));
    }
    tx.commit().await
}

/// Resolve the bootstrap principal on every static-key request so disabling the
/// user takes effect immediately.
pub async fn bootstrap_principal(
    pool: &PgPool,
    user_id: UserId,
) -> sqlx::Result<Option<Principal>> {
    let row = sqlx::query(
        "select u.id as user_id, u.username, u.display_name, pl.library_id \
         from users u \
         join personal_libraries pl on pl.user_id = u.id \
         join library l on l.id = pl.library_id and l.kind = pl.library_kind \
         where u.id = $1 and u.disabled_at is null and l.kind = 'personal'",
    )
    .bind(user_id.get())
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(principal_from_row).transpose()
}

/// Resolve a user-owned key from its SHA-256 digest. No positive result is
/// cached: revocation and user disablement apply on the next request.
pub async fn authenticate_api_key(
    pool: &PgPool,
    token_hash: &[u8],
) -> sqlx::Result<Option<(ApiKeyId, Principal, Permissions)>> {
    let row = sqlx::query(
        "select k.id as api_key_id, u.id as user_id, u.username, u.display_name, pl.library_id, \
                p.library, p.notes, p.write, p.files \
         from api_keys k \
         join users u on u.id = k.user_id \
         join personal_libraries pl on pl.user_id = u.id \
         join library l on l.id = pl.library_id and l.kind = pl.library_kind \
         join api_key_user_permissions p on p.api_key_id = k.id \
         where k.token_hash = $1 and k.revoked_at is null \
           and u.disabled_at is null and l.kind = 'personal'",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await?;
    row.as_ref()
        .map(|row| {
            let raw_api_key_id = row.get::<i64, _>("api_key_id");
            let api_key_id = ApiKeyId::new(raw_api_key_id).ok_or_else(|| {
                sqlx::Error::Protocol(format!("invalid API key ID {raw_api_key_id}"))
            })?;
            Ok((
                api_key_id,
                principal_from_row(row)?,
                Permissions {
                    library: row.get("library"),
                    notes: row.get("notes"),
                    write: row.get("write"),
                    files: row.get("files"),
                },
            ))
        })
        .transpose()
}

/// A write that changes nothing (empty batch / no matching keys): report the
/// current version, or a conflict if the client's expectation is already stale,
/// without bumping the version — a no-op must not churn the counter and make
/// every other client think the library changed.
pub(super) async fn no_change(
    pool: &PgPool,
    library_id: LibraryId,
    expected: Option<i64>,
) -> sqlx::Result<Outcome<i64>> {
    let current = current_version(pool, library_id).await?;
    Ok(match expected {
        Some(v) if v != current => Outcome::Conflict(current),
        _ => Outcome::Done(current),
    })
}

pub async fn current_version(pool: &PgPool, library_id: LibraryId) -> sqlx::Result<i64> {
    let row = sqlx::query("select version from library where id = $1")
        .bind(library_id.get())
        .fetch_one(pool)
        .await?;
    Ok(row.get("version"))
}

/// `{key: version}` for objects of `kind` changed after `since`.
pub async fn versions(
    pool: &PgPool,
    library_id: LibraryId,
    kind: &str,
    since: i64,
    include_notes: bool,
) -> sqlx::Result<Value> {
    let rows = sqlx::query(
        "select key, version from object \
         where library_id = $1 and kind = $2 and version > $3 \
         and ($4 or kind <> 'item' or item_type is distinct from 'note')",
    )
    .bind(library_id.get())
    .bind(kind)
    .bind(since)
    .bind(include_notes)
    .fetch_all(pool)
    .await?;
    Ok(version_map(rows, "key"))
}

/// `{key: version}` for top-level items changed after `since`. The client's
/// sync fetches top-level items first (a parent-first phase), so this is the
/// top-filtered counterpart of `versions(pool, library_id, "item", since)`.
pub async fn top_versions(
    pool: &PgPool,
    library_id: LibraryId,
    since: i64,
    include_notes: bool,
) -> sqlx::Result<Value> {
    let rows = sqlx::query(
        "select key, version from object \
         where library_id = $1 and kind = 'item' and is_top and version > $2 \
         and ($3 or item_type is distinct from 'note')",
    )
    .bind(library_id.get())
    .bind(since)
    .bind(include_notes)
    .fetch_all(pool)
    .await?;
    Ok(version_map(rows, "key"))
}

/// `[{key, version, data}]` for the requested keys.
pub async fn objects(
    pool: &PgPool,
    library_id: LibraryId,
    kind: &str,
    keys: &[String],
    include_notes: bool,
) -> sqlx::Result<Value> {
    let rows = sqlx::query(
        "select key, version, data from object \
         where library_id = $1 and kind = $2 and key = any($3) \
         and ($4 or kind <> 'item' or item_type is distinct from 'note')",
    )
    .bind(library_id.get())
    .bind(kind)
    .bind(keys)
    .bind(include_notes)
    .fetch_all(pool)
    .await?;
    Ok(Value::Array(rows.into_iter().map(object_json).collect()))
}

/// Escape LIKE/ILIKE wildcards so a search term matches literally; the `escape
/// '\'` clause in the query below makes `\` the escape character.
fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Append the shared `library_id … and <filters>` predicate for an item query to
/// `sql` (binding its parameters). Used by the count, page and key queries so
/// they always filter identically. Filters compare the generated columns from
/// migration 0006 (item_type/is_top/deleted/search_text/tag_names/…), so they
/// are plain indexed comparisons rather than jsonb digging.
fn push_item_filters(
    sql: &mut QueryBuilder<Postgres>,
    library_id: LibraryId,
    q: &ItemQuery,
    include_notes: bool,
) {
    sql.push("library_id = ")
        .push_bind(library_id.get())
        .push(" and kind = 'item'");

    // Zotero's notes capability hides note items from every item search shape.
    // Annotations remain visible: upstream treats only itemType=note as scoped.
    if !include_notes {
        sql.push(" and item_type is distinct from 'note'");
    }

    // Trash handling: /items/trash returns only trashed items; otherwise trashed
    // items are excluded unless includeTrashed is set.
    if q.only_trashed {
        sql.push(" and deleted");
    } else if !q.include_trashed {
        sql.push(" and not deleted");
    }

    if let Some(term) = &q.q {
        let like = format!("%{}%", escape_like(term));
        // titleCreatorYear matches title/date/creator names via the search_text
        // column (trgm-indexed); everything also matches stored full-text content.
        // That OR spans the fulltext table, so that mode cannot use the index.
        sql.push(" and (search_text ilike ")
            .push_bind(like.clone())
            .push(" escape '\\'");
        if q.qmode == QMode::Everything {
            sql.push(
                " or exists (select 1 from fulltext f \
                 where f.library_id = object.library_id and f.item_key = object.key \
                 and f.content ilike ",
            )
            .push_bind(like.clone())
            .push(" escape '\\')");
        }
        sql.push(")");
    }

    if !q.item_type.include.is_empty() {
        sql.push(" and item_type = any(")
            .push_bind(q.item_type.include.clone())
            .push(")");
    }
    if !q.item_type.exclude.is_empty() {
        sql.push(" and item_type <> all(")
            .push_bind(q.item_type.exclude.clone())
            .push(")");
    }

    // AND across groups, OR within a group: the item must carry a tag from each
    // group, i.e. tag_names overlaps every group's alternatives.
    for group in &q.tags {
        sql.push(" and tag_names && ").push_bind(group.clone());
    }

    // /items/top: only top-level items.
    if q.top {
        sql.push(" and is_top");
    }

    // /collections/<key>/items: items whose collections contain the key.
    if let Some(key) = &q.collection {
        sql.push(" and collection_keys @> ")
            .push_bind(vec![key.clone()]);
    }
}

/// Run a CLI-facing item listing: filter, search, sort and page over the stored
/// items. Returns `([{key, version, data}], total)`, where `total` is the full
/// match count. It is counted separately from the page so it stays correct even
/// when `start` runs past the end (a window count would vanish with the rows).
pub async fn query_items(
    pool: &PgPool,
    library_id: LibraryId,
    q: &ItemQuery,
    include_notes: bool,
) -> sqlx::Result<(Vec<Value>, i64)> {
    let mut count: QueryBuilder<Postgres> = QueryBuilder::new("select count(*) from object where ");
    push_item_filters(&mut count, library_id, q, include_notes);
    let total: i64 = count.build().fetch_one(pool).await?.get(0);

    let mut sql: QueryBuilder<Postgres> =
        QueryBuilder::new("select key, version, data from object where ");
    push_item_filters(&mut sql, library_id, q, include_notes);
    // order_expr/sql() are fixed strings (no user input), so pushing them raw is
    // safe; nulls sort last so items missing the sort field don't lead.
    sql.push(" order by ")
        .push(q.sort.order_expr())
        .push(" ")
        .push(q.direction.sql())
        .push(" nulls last, key asc limit ")
        .push_bind(q.limit)
        .push(" offset ")
        .push_bind(q.start);

    let items = sql
        .build()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(object_json)
        .collect();
    Ok((items, total))
}

/// Every item key matching the query, ordered, with no paging — the plain key
/// list the sync client's `getKeys()` consumes (e.g. the top-level items of a
/// restored collection). Shares `push_item_filters`, so `format=keys` honours
/// the same filters as the JSON listing.
pub async fn item_keys(
    pool: &PgPool,
    library_id: LibraryId,
    q: &ItemQuery,
    include_notes: bool,
) -> sqlx::Result<Vec<String>> {
    let mut sql: QueryBuilder<Postgres> = QueryBuilder::new("select key from object where ");
    push_item_filters(&mut sql, library_id, q, include_notes);
    sql.push(" order by key");
    let keys = sql
        .build()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| row.get::<String, _>("key"))
        .collect();
    Ok(keys)
}

/// Store a batch, stamping each object with the new library version. Both `POST`
/// and `PATCH` create-or-update with **merge** semantics: each object's provided
/// top-level fields are overlaid onto the existing stored object, omitted fields
/// are kept, and an empty value clears a field. This matches the Zotero sync
/// client, which uploads only the *changed* fields of an existing object via
/// `POST` (e.g. just `lastRead` after opening an attachment) and relies on the
/// server preserving the rest — a full replace would drop `itemType`/`linkMode`
/// and corrupt the object. Returns `(new_version, successful_map)` keyed by index.
pub async fn write(
    pool: &PgPool,
    library_id: LibraryId,
    kind: &str,
    batch: Vec<Value>,
    expected: Option<i64>,
    allow_stored_file_write: bool,
) -> sqlx::Result<ObjectMutation<(i64, Value)>> {
    if batch.is_empty() {
        return Ok(match no_change(pool, library_id, expected).await? {
            Outcome::Done(version) => ObjectMutation::Done((version, Value::Object(Map::new()))),
            Outcome::Conflict(current) => ObjectMutation::Conflict(current),
        });
    }
    let mut tx = pool.begin().await?;
    let version = match guarded_version(&mut tx, library_id, expected).await? {
        Outcome::Done(version) => version,
        Outcome::Conflict(current) => return Ok(ObjectMutation::Conflict(current)),
    };

    let mut successful = Map::new();
    for (index, provided) in batch.into_iter().enumerate() {
        let key = provided
            .get("key")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(generate_key);
        // Start from the existing object (read in this txn so the version guard
        // serialises it) and overlay the provided top-level fields; a new key has
        // nothing to merge into, so the provided object is stored as-is.
        let existing =
            sqlx::query("select data from object where library_id = $1 and kind = $2 and key = $3")
                .bind(library_id.get())
                .bind(kind)
                .bind(&key)
                .fetch_optional(&mut *tx)
                .await?
                .map(|row| row.get::<Value, _>("data"));
        let existing_is_stored_file = existing.as_ref().is_some_and(is_stored_file_attachment);
        let mut object = match (existing, &provided) {
            (Some(Value::Object(mut base)), Value::Object(fields)) => {
                for (k, v) in fields {
                    base.insert(k.clone(), v.clone());
                }
                Value::Object(base)
            }
            _ => provided,
        };
        if let Value::Object(fields) = &mut object {
            fields.insert("key".into(), Value::from(key.clone()));
            fields.insert("version".into(), Value::from(version));
        }
        let object_is_stored_file = is_stored_file_attachment(&object);
        if kind == "item"
            && !allow_stored_file_write
            && (existing_is_stored_file || object_is_stored_file)
        {
            return Ok(ObjectMutation::FileWriteDenied);
        }
        sqlx::query(
            "insert into object (library_id, kind, key, version, data) \
             values ($1, $2, $3, $4, $5) \
             on conflict (library_id, kind, key) \
             do update set version = $4, data = $5",
        )
        .bind(library_id.get())
        .bind(kind)
        .bind(&key)
        .bind(version)
        .bind(&object)
        .execute(&mut *tx)
        .await?;
        if kind == "item" && !(existing_is_stored_file && object_is_stored_file) {
            // File metadata is valid only while one stored attachment identity
            // remains continuous. Reusing a deleted or converted key must not
            // resurrect bytes registered for its previous identity.
            sqlx::query("delete from file where library_id = $1 and item_key = $2")
                .bind(library_id.get())
                .bind(&key)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("delete from deletion where library_id = $1 and kind = $2 and key = $3")
            .bind(library_id.get())
            .bind(kind)
            .bind(&key)
            .execute(&mut *tx)
            .await?;
        successful.insert(
            index.to_string(),
            serde_json::json!({ "key": key, "version": version, "data": order_fields(object) }),
        );
    }
    tx.commit().await?;
    Ok(ObjectMutation::Done((version, Value::Object(successful))))
}

/// Delete objects of `kind`, recording them in the deletion log.
pub async fn delete(
    pool: &PgPool,
    library_id: LibraryId,
    kind: &str,
    keys: &[String],
    expected: Option<i64>,
    allow_stored_file_write: bool,
) -> sqlx::Result<ObjectMutation<i64>> {
    if keys.is_empty() {
        return Ok(match no_change(pool, library_id, expected).await? {
            Outcome::Done(version) => ObjectMutation::Done(version),
            Outcome::Conflict(current) => ObjectMutation::Conflict(current),
        });
    }
    let mut tx = pool.begin().await?;
    let version = match guarded_version(&mut tx, library_id, expected).await? {
        Outcome::Done(version) => version,
        Outcome::Conflict(current) => return Ok(ObjectMutation::Conflict(current)),
    };
    for key in keys {
        if kind == "item" && !allow_stored_file_write {
            let data: Option<Value> = sqlx::query_scalar(
                "select data from object where library_id = $1 and kind = $2 and key = $3",
            )
            .bind(library_id.get())
            .bind(kind)
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
            if data.as_ref().is_some_and(is_stored_file_attachment) {
                return Ok(ObjectMutation::FileWriteDenied);
            }
        }
        sqlx::query("delete from object where library_id = $1 and kind = $2 and key = $3")
            .bind(library_id.get())
            .bind(kind)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        if kind == "item" {
            sqlx::query("delete from file where library_id = $1 and item_key = $2")
                .bind(library_id.get())
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "insert into deletion (library_id, kind, key, version) values ($1, $2, $3, $4) \
             on conflict (library_id, kind, key) do update set version = $4",
        )
        .bind(library_id.get())
        .bind(kind)
        .bind(key)
        .bind(version)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(ObjectMutation::Done(version))
}

pub struct FileMeta {
    pub md5: String,
    pub blob_md5: String,
    pub mtime: i64,
    pub filesize: i64,
    pub compressed: bool,
    pub blob_key: String,
}

/// Client-visible metadata and immutable object pointer for an attachment file.
pub async fn file_meta(
    pool: &PgPool,
    library_id: LibraryId,
    item_key: &str,
) -> sqlx::Result<Option<FileMeta>> {
    let row = sqlx::query(
        "select f.md5, f.blob_md5, f.mtime, f.filesize, f.compressed, f.blob_key \
         from file f \
         join object o on o.library_id = f.library_id \
             and o.kind = 'item' \
             and o.key = f.item_key \
             and o.data->>'itemType' = 'attachment' \
             and o.data->>'linkMode' in ('imported_file', 'imported_url') \
         where f.library_id = $1 and f.item_key = $2",
    )
    .bind(library_id.get())
    .bind(item_key)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| FileMeta {
        md5: r.get("md5"),
        blob_md5: r.get("blob_md5"),
        mtime: r.get("mtime"),
        filesize: r.get("filesize"),
        compressed: r.get("compressed"),
        blob_key: r.get("blob_key"),
    }))
}

pub async fn stored_file_attachment_exists(
    pool: &PgPool,
    library_id: LibraryId,
    item_key: &str,
) -> sqlx::Result<bool> {
    let data: Option<Value> = sqlx::query_scalar(
        "select data from object where library_id = $1 and kind = 'item' and key = $2",
    )
    .bind(library_id.get())
    .bind(item_key)
    .fetch_optional(pool)
    .await?;
    Ok(data.as_ref().is_some_and(is_stored_file_attachment))
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum PendingUploadState {
    Authorized,
    Uploading,
    Uploaded,
    Discarded,
    Deleting,
}

#[derive(Clone)]
pub struct PendingUpload {
    pub library_id: LibraryId,
    pub group_id: Option<GroupId>,
    pub authorizer_key_hash: Vec<u8>,
    pub bootstrap_user_id: Option<UserId>,
    pub item_key: String,
    pub expected_md5: Option<String>,
    pub blob_key: String,
    pub md5: String,
    pub upload_md5: String,
    pub filename: String,
    pub filesize: i64,
    pub mtime: i64,
    pub compressed: bool,
    pub state: PendingUploadState,
}

const PENDING_UPLOAD_COLUMNS: &str = "\
    library_id, group_id, authorizer_key_hash, bootstrap_user_id, \
    item_key, expected_md5, blob_key, md5, upload_md5, filename, filesize, \
    mtime, compressed, state";

fn pending_upload_from_row(row: PgRow) -> PendingUpload {
    let bootstrap_user_id = row
        .get::<Option<i64>, _>("bootstrap_user_id")
        .map(|id| UserId::new(id).expect("pending upload user IDs are positive"));
    PendingUpload {
        library_id: LibraryId::new(row.get("library_id"))
            .expect("pending upload library IDs are positive"),
        group_id: row
            .get::<Option<i64>, _>("group_id")
            .map(|id| GroupId::new(id).expect("pending upload group IDs are positive")),
        authorizer_key_hash: row.get("authorizer_key_hash"),
        bootstrap_user_id,
        item_key: row.get("item_key"),
        expected_md5: row.get("expected_md5"),
        blob_key: row.get("blob_key"),
        md5: row.get("md5"),
        upload_md5: row.get("upload_md5"),
        filename: row.get("filename"),
        filesize: row.get("filesize"),
        mtime: row.get("mtime"),
        compressed: row.get("compressed"),
        state: match row.get::<String, _>("state").as_str() {
            "authorized" => PendingUploadState::Authorized,
            "uploading" => PendingUploadState::Uploading,
            "uploaded" => PendingUploadState::Uploaded,
            "discarded" => PendingUploadState::Discarded,
            "deleting" => PendingUploadState::Deleting,
            _ => unreachable!("pending upload state constraint"),
        },
    }
}

pub async fn create_pending_upload(
    pool: &PgPool,
    token_hash: &[u8],
    upload: &PendingUpload,
) -> sqlx::Result<()> {
    sqlx::query(
        "insert into pending_uploads ( \
             token_hash, library_id, group_id, authorizer_key_hash, bootstrap_user_id, \
             item_key, expected_md5, blob_key, md5, upload_md5, filename, \
             filesize, mtime, compressed, state \
         ) values ( \
             $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, \
             'authorized' \
         )",
    )
    .bind(token_hash)
    .bind(upload.library_id.get())
    .bind(upload.group_id.map(GroupId::get))
    .bind(&upload.authorizer_key_hash)
    .bind(upload.bootstrap_user_id.map(UserId::get))
    .bind(&upload.item_key)
    .bind(&upload.expected_md5)
    .bind(&upload.blob_key)
    .bind(&upload.md5)
    .bind(&upload.upload_md5)
    .bind(&upload.filename)
    .bind(upload.filesize)
    .bind(upload.mtime)
    .bind(upload.compressed)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn pending_upload(
    pool: &PgPool,
    token_hash: &[u8],
) -> sqlx::Result<Option<PendingUpload>> {
    let query = format!(
        "select {PENDING_UPLOAD_COLUMNS} \
         from pending_uploads where token_hash = $1 and expires_at > now()"
    );
    let row = sqlx::query(&query)
        .bind(token_hash)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(pending_upload_from_row))
}

pub async fn claim_pending_upload(pool: &PgPool, token_hash: &[u8]) -> sqlx::Result<bool> {
    let changed = sqlx::query(
        "update pending_uploads set state = 'uploading' \
         where token_hash = $1 and state = 'authorized' and expires_at > now()",
    )
    .bind(token_hash)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

pub async fn reset_pending_upload(pool: &PgPool, token_hash: &[u8]) -> sqlx::Result<()> {
    sqlx::query(
        "update pending_uploads \
         set state = case when expires_at > now() then 'authorized' else 'discarded' end \
         where token_hash = $1 and state = 'uploading'",
    )
    .bind(token_hash)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn discard_pending_upload(pool: &PgPool, token_hash: &[u8]) -> sqlx::Result<()> {
    sqlx::query(
        "update pending_uploads set state = 'discarded' \
         where token_hash = $1 and state = 'uploading'",
    )
    .bind(token_hash)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_pending_upload_uploaded(pool: &PgPool, token_hash: &[u8]) -> sqlx::Result<bool> {
    let changed = sqlx::query(
        "update pending_uploads set state = 'uploaded' \
         where token_hash = $1 and state = 'uploading' and expires_at > now()",
    )
    .bind(token_hash)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(changed == 1)
}

pub async fn reset_interrupted_pending_uploads(pool: &PgPool) -> sqlx::Result<()> {
    sqlx::query("update pending_uploads set state = 'authorized' where state = 'uploading'")
        .execute(pool)
        .await?;
    Ok(())
}

pub struct PendingUploadGarbage {
    pub token_hash: Vec<u8>,
    pub blob_key: String,
}

pub async fn claim_pending_upload_garbage(
    pool: &PgPool,
    limit: i64,
) -> sqlx::Result<Vec<PendingUploadGarbage>> {
    let rows = sqlx::query(
        "with candidates as ( \
             select token_hash from pending_uploads \
             where state = 'discarded' \
                or (expires_at <= now() and state not in ('uploading', 'deleting')) \
                or (state = 'deleting' \
                    and coalesce(gc_attempted_at, '-infinity') \
                        < now() - interval '1 minute') \
             order by expires_at limit $1 for update skip locked \
         ) \
         update pending_uploads p set state = 'deleting', gc_attempted_at = now() \
         from candidates c where p.token_hash = c.token_hash \
         returning p.token_hash, p.blob_key",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| PendingUploadGarbage {
            token_hash: row.get("token_hash"),
            blob_key: row.get("blob_key"),
        })
        .collect())
}

pub async fn delete_pending_upload_garbage(pool: &PgPool, token_hash: &[u8]) -> sqlx::Result<()> {
    sqlx::query("delete from pending_uploads where token_hash = $1 and state = 'deleting'")
        .bind(token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

pub enum FileRegistrationOutcome {
    Done(i64),
    Conflict(i64),
    InvalidItem,
    InvalidUpload,
}

/// Atomically make one immutable candidate the registered attachment and consume
/// its capability. Library serialization makes the file compare-and-swap safe
/// for both existing rows and concurrent first uploads.
pub async fn register_file(
    pool: &PgPool,
    token_hash: &[u8],
    library_id: LibraryId,
    item_key: &str,
) -> sqlx::Result<FileRegistrationOutcome> {
    let mut tx = pool.begin().await?;
    let query = format!(
        "select {PENDING_UPLOAD_COLUMNS}, expires_at > now() as valid \
         from pending_uploads where token_hash = $1 for update"
    );
    let pending = sqlx::query(&query)
        .bind(token_hash)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(pending) = pending else {
        return Ok(FileRegistrationOutcome::InvalidUpload);
    };
    let valid = pending.get::<bool, _>("valid");
    let upload = pending_upload_from_row(pending);
    if !valid {
        sqlx::query("update pending_uploads set state = 'discarded' where token_hash = $1")
            .bind(token_hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(FileRegistrationOutcome::InvalidUpload);
    }
    if upload.state != PendingUploadState::Uploaded
        || upload.library_id != library_id
        || upload.item_key != item_key
    {
        return Ok(FileRegistrationOutcome::InvalidUpload);
    }

    let current_version: i64 =
        sqlx::query_scalar("select version from library where id = $1 for update")
            .bind(library_id.get())
            .fetch_one(&mut *tx)
            .await?;
    let item: Option<Value> = sqlx::query_scalar(
        "select data from object \
         where library_id = $1 and kind = 'item' and key = $2 for update",
    )
    .bind(library_id.get())
    .bind(item_key)
    .fetch_optional(&mut *tx)
    .await?;
    if item
        .as_ref()
        .is_none_or(|data| !is_stored_file_attachment(data))
    {
        sqlx::query("update pending_uploads set state = 'discarded' where token_hash = $1")
            .bind(token_hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(FileRegistrationOutcome::InvalidItem);
    }
    let current_md5 = sqlx::query_scalar::<_, String>(
        "select md5 from file where library_id = $1 and item_key = $2",
    )
    .bind(library_id.get())
    .bind(item_key)
    .fetch_optional(&mut *tx)
    .await?;
    let precondition_matches = match (upload.expected_md5.as_deref(), current_md5.as_deref()) {
        (None, None) => true,
        (Some(expected), Some(current)) => expected.eq_ignore_ascii_case(current),
        _ => false,
    };
    if !precondition_matches {
        sqlx::query("update pending_uploads set state = 'discarded' where token_hash = $1")
            .bind(token_hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(FileRegistrationOutcome::Conflict(current_version));
    }
    let version = current_version + 1;
    sqlx::query("update library set version = $2 where id = $1")
        .bind(library_id.get())
        .bind(version)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "insert into file \
         (library_id, item_key, blob_key, md5, blob_md5, filename, filesize, mtime, \
          compressed, version) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         on conflict (library_id, item_key) \
         do update set blob_key = $3, md5 = $4, blob_md5 = $5, filename = $6, \
                       filesize = $7, mtime = $8, compressed = $9, version = $10",
    )
    .bind(library_id.get())
    .bind(item_key)
    .bind(&upload.blob_key)
    .bind(&upload.md5)
    .bind(&upload.upload_md5)
    .bind(&upload.filename)
    .bind(upload.filesize)
    .bind(upload.mtime)
    .bind(upload.compressed)
    .bind(version)
    .execute(&mut *tx)
    .await?;
    // Real Zotero stamps original-file metadata onto the attachment item when
    // registering a ZIP. Clients use filename to select its primary ZIP entry.
    sqlx::query(
        "update object set version = $3, \
         data = data || jsonb_build_object( \
             'md5', $4::text, 'mtime', $5::bigint, 'filename', $6::text, 'version', $3) \
         where library_id = $1 and kind = 'item' and key = $2",
    )
    .bind(library_id.get())
    .bind(item_key)
    .bind(version)
    .bind(&upload.md5)
    .bind(upload.mtime)
    .bind(&upload.filename)
    .execute(&mut *tx)
    .await?;
    sqlx::query("delete from pending_uploads where token_hash = $1")
        .bind(token_hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(FileRegistrationOutcome::Done(version))
}

/// Deleted object keys after `since`, grouped by kind for the /deleted endpoint.
pub async fn deleted(pool: &PgPool, library_id: LibraryId, since: i64) -> sqlx::Result<Value> {
    let rows = sqlx::query("select kind, key from deletion where library_id = $1 and version > $2")
        .bind(library_id.get())
        .bind(since)
        .fetch_all(pool)
        .await?;
    let (mut collections, mut searches, mut items, mut settings, mut tags) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for row in rows {
        let key: String = row.get("key");
        match row.get::<String, _>("kind").as_str() {
            "collection" => collections.push(Value::from(key)),
            "search" => searches.push(Value::from(key)),
            "setting" => settings.push(Value::from(key)),
            // Tag deletions are objects ({tag, type}); type is unknown here, so 0.
            "tag" => tags.push(serde_json::json!({ "tag": key, "type": 0 })),
            _ => items.push(Value::from(key)),
        }
    }
    Ok(serde_json::json!({
        "collections": collections,
        "searches": searches,
        "items": items,
        "settings": settings,
        "tags": tags,
    }))
}
