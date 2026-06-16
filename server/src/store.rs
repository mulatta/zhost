//! PostgreSQL-backed library storage.
//!
//! Objects are stored as opaque jsonb blobs keyed by (kind, key). Every write
//! bumps the single library version counter inside a transaction and stamps the
//! affected rows with the new version, so the client's `since` reads and
//! `If-Unmodified-Since-Version` writes stay coherent.

use serde_json::{Map, Value};
use sqlx::{PgPool, Row};

/// Single user, single personal library.
const LIBRARY_ID: i64 = 1;

/// Zotero's attachment `fromJSON` processes fields in object order and requires
/// `linkMode` before `filename`/`path`. jsonb storage sorts keys alphabetically
/// (filename < linkMode), so re-emit attachment data with linkMode first. Relies
/// on serde_json's preserve_order feature.
fn linkmode_first(data: Value) -> Value {
    let Value::Object(map) = &data else {
        return data;
    };
    if !map.contains_key("linkMode") {
        return data;
    }
    let mut ordered = Map::new();
    if let Some(link_mode) = map.get("linkMode") {
        ordered.insert("linkMode".into(), link_mode.clone());
    }
    for (key, value) in map {
        if key != "linkMode" {
            ordered.insert(key.clone(), value.clone());
        }
    }
    Value::Object(ordered)
}

/// A write either committed at a new version or was rejected because the
/// client's `If-Unmodified-Since-Version` no longer matches the library.
pub enum Outcome<T> {
    Done(T),
    Conflict(i64),
}

/// Lock the library row, check the client's expected version, and reserve the
/// next one. Serializing on the row also prevents concurrent writes from racing.
async fn guarded_version(
    conn: &mut sqlx::PgConnection,
    expected: Option<i64>,
) -> sqlx::Result<Outcome<i64>> {
    let current: i64 = sqlx::query("select version from library where id = $1 for update")
        .bind(LIBRARY_ID)
        .fetch_one(&mut *conn)
        .await?
        .get("version");
    if matches!(expected, Some(v) if v != current) {
        return Ok(Outcome::Conflict(current));
    }
    let version = current + 1;
    sqlx::query("update library set version = $2 where id = $1")
        .bind(LIBRARY_ID)
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

pub async fn current_version(pool: &PgPool) -> sqlx::Result<i64> {
    let row = sqlx::query("select version from library where id = $1")
        .bind(LIBRARY_ID)
        .fetch_one(pool)
        .await?;
    Ok(row.get("version"))
}

/// `{key: version}` for objects of `kind` changed after `since`.
pub async fn versions(pool: &PgPool, kind: &str, since: i64) -> sqlx::Result<Value> {
    let rows = sqlx::query(
        "select key, version from object \
         where library_id = $1 and kind = $2 and version > $3",
    )
    .bind(LIBRARY_ID)
    .bind(kind)
    .bind(since)
    .fetch_all(pool)
    .await?;
    let mut map = Map::new();
    for row in rows {
        map.insert(row.get("key"), Value::from(row.get::<i64, _>("version")));
    }
    Ok(Value::Object(map))
}

/// `[{key, version, data}]` for the requested keys.
pub async fn objects(pool: &PgPool, kind: &str, keys: &[String]) -> sqlx::Result<Value> {
    let rows = sqlx::query(
        "select key, version, data from object \
         where library_id = $1 and kind = $2 and key = any($3)",
    )
    .bind(LIBRARY_ID)
    .bind(kind)
    .bind(keys)
    .fetch_all(pool)
    .await?;
    let array = rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "key": row.get::<String, _>("key"),
                "version": row.get::<i64, _>("version"),
                "data": linkmode_first(row.get::<Value, _>("data")),
            })
        })
        .collect();
    Ok(Value::Array(array))
}

/// Store a batch verbatim, stamping each object with the new library version.
/// Returns `(new_version, successful_map)` where the map is keyed by batch index.
pub async fn write(
    pool: &PgPool,
    kind: &str,
    batch: Vec<Value>,
    expected: Option<i64>,
) -> sqlx::Result<Outcome<(i64, Value)>> {
    let mut tx = pool.begin().await?;
    let version = match guarded_version(&mut tx, expected).await? {
        Outcome::Done(version) => version,
        Outcome::Conflict(current) => return Ok(Outcome::Conflict(current)),
    };

    let mut successful = Map::new();
    for (index, mut object) in batch.into_iter().enumerate() {
        let key = object
            .get("key")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("ZH{index:06}"));
        if let Value::Object(fields) = &mut object {
            fields.insert("key".into(), Value::from(key.clone()));
            fields.insert("version".into(), Value::from(version));
        }
        sqlx::query(
            "insert into object (library_id, kind, key, version, data) \
             values ($1, $2, $3, $4, $5) \
             on conflict (library_id, kind, key) \
             do update set version = $4, data = $5",
        )
        .bind(LIBRARY_ID)
        .bind(kind)
        .bind(&key)
        .bind(version)
        .bind(&object)
        .execute(&mut *tx)
        .await?;
        sqlx::query("delete from deletion where library_id = $1 and kind = $2 and key = $3")
            .bind(LIBRARY_ID)
            .bind(kind)
            .bind(&key)
            .execute(&mut *tx)
            .await?;
        successful.insert(
            index.to_string(),
            serde_json::json!({ "key": key, "version": version, "data": linkmode_first(object) }),
        );
    }
    tx.commit().await?;
    Ok(Outcome::Done((version, Value::Object(successful))))
}

/// Delete objects of `kind`, recording them in the deletion log.
pub async fn delete(
    pool: &PgPool,
    kind: &str,
    keys: &[String],
    expected: Option<i64>,
) -> sqlx::Result<Outcome<i64>> {
    let mut tx = pool.begin().await?;
    let version = match guarded_version(&mut tx, expected).await? {
        Outcome::Done(version) => version,
        Outcome::Conflict(current) => return Ok(Outcome::Conflict(current)),
    };
    for key in keys {
        sqlx::query("delete from object where library_id = $1 and kind = $2 and key = $3")
            .bind(LIBRARY_ID)
            .bind(kind)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into deletion (library_id, kind, key, version) values ($1, $2, $3, $4) \
             on conflict (library_id, kind, key) do update set version = $4",
        )
        .bind(LIBRARY_ID)
        .bind(kind)
        .bind(key)
        .bind(version)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Outcome::Done(version))
}

/// All settings as `{key: {value, version}}`.
pub async fn settings(pool: &PgPool) -> sqlx::Result<Value> {
    let rows = sqlx::query("select key, version, value from setting where library_id = $1")
        .bind(LIBRARY_ID)
        .fetch_all(pool)
        .await?;
    let mut map = Map::new();
    for row in rows {
        let value: Value = row.get("value");
        map.insert(
            row.get("key"),
            serde_json::json!({ "value": value, "version": row.get::<i64, _>("version") }),
        );
    }
    Ok(Value::Object(map))
}

/// Store a `{key: {value}}` settings object.
pub async fn write_settings(
    pool: &PgPool,
    body: Value,
    expected: Option<i64>,
) -> sqlx::Result<Outcome<i64>> {
    let mut tx = pool.begin().await?;
    let version = match guarded_version(&mut tx, expected).await? {
        Outcome::Done(version) => version,
        Outcome::Conflict(current) => return Ok(Outcome::Conflict(current)),
    };
    if let Value::Object(entries) = body {
        for (key, entry) in entries {
            let value = entry.get("value").cloned().unwrap_or(entry);
            sqlx::query(
                "insert into setting (library_id, key, version, value) values ($1, $2, $3, $4) \
                 on conflict (library_id, key) do update set version = $3, value = $4",
            )
            .bind(LIBRARY_ID)
            .bind(&key)
            .bind(version)
            .bind(&value)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(Outcome::Done(version))
}

/// Whether an attachment file with this md5 is already registered.
pub async fn file_exists(pool: &PgPool, item_key: &str, md5: &str) -> sqlx::Result<bool> {
    let row =
        sqlx::query("select 1 from file where library_id = $1 and item_key = $2 and md5 = $3")
            .bind(LIBRARY_ID)
            .bind(item_key)
            .bind(md5)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

/// md5, filename, mtime for an attachment file, if registered.
pub async fn file_meta(
    pool: &PgPool,
    item_key: &str,
) -> sqlx::Result<Option<(String, String, i64)>> {
    let row = sqlx::query(
        "select md5, filename, mtime from file where library_id = $1 and item_key = $2",
    )
    .bind(LIBRARY_ID)
    .bind(item_key)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.get("md5"), r.get("filename"), r.get("mtime"))))
}

/// Register an uploaded attachment file, bumping the library version.
pub async fn register_file(
    pool: &PgPool,
    item_key: &str,
    md5: &str,
    filename: &str,
    filesize: i64,
    mtime: i64,
) -> sqlx::Result<i64> {
    let mut tx = pool.begin().await?;
    let version: i64 =
        sqlx::query("update library set version = version + 1 where id = $1 returning version")
            .bind(LIBRARY_ID)
            .fetch_one(&mut *tx)
            .await?
            .get("version");
    sqlx::query(
        "insert into file (library_id, item_key, md5, filename, filesize, mtime, version) \
         values ($1, $2, $3, $4, $5, $6, $7) \
         on conflict (library_id, item_key) \
         do update set md5 = $3, filename = $4, filesize = $5, mtime = $6, version = $7",
    )
    .bind(LIBRARY_ID)
    .bind(item_key)
    .bind(md5)
    .bind(filename)
    .bind(filesize)
    .bind(mtime)
    .bind(version)
    .execute(&mut *tx)
    .await?;
    // Real Zotero stamps md5/mtime onto the attachment item's data when the
    // file is registered; without them the downloading client can't reconcile
    // the file and rejects the attachment.
    sqlx::query(
        "update object set version = $3, \
         data = data || jsonb_build_object('md5', $4::text, 'mtime', $5::bigint, 'version', $3) \
         where library_id = $1 and kind = 'item' and key = $2",
    )
    .bind(LIBRARY_ID)
    .bind(item_key)
    .bind(version)
    .bind(md5)
    .bind(mtime)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(version)
}

/// `{itemKey: version}` for full-text content changed after `since`. The version
/// is the row's own (the library version at which it last changed), so it equals
/// the per-item download's `Last-Modified-Version` and the client skips
/// re-downloading content it already holds.
pub async fn fulltext_versions(pool: &PgPool, since: i64) -> sqlx::Result<Value> {
    let rows = sqlx::query(
        "select item_key, version from fulltext where library_id = $1 and version > $2",
    )
    .bind(LIBRARY_ID)
    .bind(since)
    .fetch_all(pool)
    .await?;
    let mut map = Map::new();
    for row in rows {
        map.insert(
            row.get("item_key"),
            Value::from(row.get::<i64, _>("version")),
        );
    }
    Ok(Value::Object(map))
}

/// Full-text content for one item as `(row_version, {content, indexedChars, …})`.
pub async fn fulltext_item(pool: &PgPool, item_key: &str) -> sqlx::Result<Option<(i64, Value)>> {
    let row = sqlx::query(
        "select content, indexed_chars, total_chars, indexed_pages, total_pages, version \
         from fulltext where library_id = $1 and item_key = $2",
    )
    .bind(LIBRARY_ID)
    .bind(item_key)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| {
        let data = serde_json::json!({
            "content": r.get::<String, _>("content"),
            "indexedChars": r.get::<i64, _>("indexed_chars"),
            "totalChars": r.get::<i64, _>("total_chars"),
            "indexedPages": r.get::<i64, _>("indexed_pages"),
            "totalPages": r.get::<i64, _>("total_pages"),
        });
        (r.get::<i64, _>("version"), data)
    }))
}

/// Store a batch of full-text content, stamping each row with the new library
/// version. Returns `(new_version, successful_map)` keyed by batch index; each
/// value carries the item `key` the client matches against to mark it synced.
pub async fn write_fulltext(
    pool: &PgPool,
    batch: Vec<Value>,
    expected: Option<i64>,
) -> sqlx::Result<Outcome<(i64, Value)>> {
    let mut tx = pool.begin().await?;
    let version = match guarded_version(&mut tx, expected).await? {
        Outcome::Done(version) => version,
        Outcome::Conflict(current) => return Ok(Outcome::Conflict(current)),
    };
    let mut successful = Map::new();
    for (index, item) in batch.into_iter().enumerate() {
        let key = item
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let content = item
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let count = |name: &str| item.get(name).and_then(Value::as_i64).unwrap_or(0);
        sqlx::query(
            "insert into fulltext \
             (library_id, item_key, content, indexed_chars, total_chars, \
              indexed_pages, total_pages, version) \
             values ($1, $2, $3, $4, $5, $6, $7, $8) \
             on conflict (library_id, item_key) \
             do update set content = $3, indexed_chars = $4, total_chars = $5, \
                indexed_pages = $6, total_pages = $7, version = $8",
        )
        .bind(LIBRARY_ID)
        .bind(&key)
        .bind(content)
        .bind(count("indexedChars"))
        .bind(count("totalChars"))
        .bind(count("indexedPages"))
        .bind(count("totalPages"))
        .bind(version)
        .execute(&mut *tx)
        .await?;
        successful.insert(index.to_string(), serde_json::json!({ "key": key }));
    }
    tx.commit().await?;
    Ok(Outcome::Done((version, Value::Object(successful))))
}

/// Deleted object keys after `since`, grouped by kind for the /deleted endpoint.
pub async fn deleted(pool: &PgPool, since: i64) -> sqlx::Result<Value> {
    let rows = sqlx::query("select kind, key from deletion where library_id = $1 and version > $2")
        .bind(LIBRARY_ID)
        .bind(since)
        .fetch_all(pool)
        .await?;
    let (mut collections, mut searches, mut items, mut tags) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for row in rows {
        let key: String = row.get("key");
        match row.get::<String, _>("kind").as_str() {
            "collection" => collections.push(Value::from(key)),
            "search" => searches.push(Value::from(key)),
            "tag" => tags.push(Value::from(key)),
            _ => items.push(Value::from(key)),
        }
    }
    Ok(serde_json::json!({
        "collections": collections,
        "searches": searches,
        "items": items,
        "tags": tags,
    }))
}
