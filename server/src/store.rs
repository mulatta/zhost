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
                "data": row.get::<Value, _>("data"),
            })
        })
        .collect();
    Ok(Value::Array(array))
}

/// Store a batch verbatim, stamping each object with the new library version.
/// Returns `(new_version, successful_map)` where the map is keyed by batch index.
pub async fn write(pool: &PgPool, kind: &str, batch: Vec<Value>) -> sqlx::Result<(i64, Value)> {
    let mut tx = pool.begin().await?;
    let version: i64 =
        sqlx::query("update library set version = version + 1 where id = $1 returning version")
            .bind(LIBRARY_ID)
            .fetch_one(&mut *tx)
            .await?
            .get("version");

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
            serde_json::json!({ "key": key, "version": version, "data": object }),
        );
    }
    tx.commit().await?;
    Ok((version, Value::Object(successful)))
}

/// Delete objects of `kind`, recording them in the deletion log. Returns the
/// new library version.
pub async fn delete(pool: &PgPool, kind: &str, keys: &[String]) -> sqlx::Result<i64> {
    let mut tx = pool.begin().await?;
    let version: i64 =
        sqlx::query("update library set version = version + 1 where id = $1 returning version")
            .bind(LIBRARY_ID)
            .fetch_one(&mut *tx)
            .await?
            .get("version");
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
    Ok(version)
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

/// Store a `{key: {value}}` settings object. Returns the new library version.
pub async fn write_settings(pool: &PgPool, body: Value) -> sqlx::Result<i64> {
    let mut tx = pool.begin().await?;
    let version: i64 =
        sqlx::query("update library set version = version + 1 where id = $1 returning version")
            .bind(LIBRARY_ID)
            .fetch_one(&mut *tx)
            .await?
            .get("version");
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
    Ok(version)
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
