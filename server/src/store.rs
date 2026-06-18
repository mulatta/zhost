//! PostgreSQL-backed library storage.
//!
//! Objects are stored as opaque jsonb blobs keyed by (kind, key). Every write
//! bumps the single library version counter inside a transaction and stamps the
//! affected rows with the new version, so the client's `since` reads and
//! `If-Unmodified-Since-Version` writes stay coherent.

use serde_json::{Map, Value};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

use crate::query::{ItemQuery, QMode};

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

/// `{key: version}` for top-level items changed after `since`. The client's
/// sync fetches top-level items first (a parent-first phase), so this is the
/// top-filtered counterpart of `versions(pool, "item", since)`.
pub async fn top_versions(pool: &PgPool, since: i64) -> sqlx::Result<Value> {
    let rows = sqlx::query(
        "select key, version from object \
         where library_id = $1 and kind = 'item' and is_top and version > $2",
    )
    .bind(LIBRARY_ID)
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
fn push_item_filters(sql: &mut QueryBuilder<Postgres>, q: &ItemQuery) {
    sql.push("library_id = ")
        .push_bind(LIBRARY_ID)
        .push(" and kind = 'item'");

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
pub async fn query_items(pool: &PgPool, q: &ItemQuery) -> sqlx::Result<(Vec<Value>, i64)> {
    let mut count: QueryBuilder<Postgres> = QueryBuilder::new("select count(*) from object where ");
    push_item_filters(&mut count, q);
    let total: i64 = count.build().fetch_one(pool).await?.get(0);

    let mut sql: QueryBuilder<Postgres> =
        QueryBuilder::new("select key, version, data from object where ");
    push_item_filters(&mut sql, q);
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
        .map(|row| {
            serde_json::json!({
                "key": row.get::<String, _>("key"),
                "version": row.get::<i64, _>("version"),
                "data": linkmode_first(row.get::<Value, _>("data")),
            })
        })
        .collect();
    Ok((items, total))
}

/// Every item key matching the query, ordered, with no paging — the plain key
/// list the sync client's `getKeys()` consumes (e.g. the top-level items of a
/// restored collection). Shares `push_item_filters`, so `format=keys` honours
/// the same filters as the JSON listing.
pub async fn item_keys(pool: &PgPool, q: &ItemQuery) -> sqlx::Result<Vec<String>> {
    let mut sql: QueryBuilder<Postgres> = QueryBuilder::new("select key from object where ");
    push_item_filters(&mut sql, q);
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

/// Distinct tags across non-trashed items with their item counts, as
/// `[{tag, numItems}]` ordered by tag. Backs the CLI-facing `/tags` listing.
pub async fn tags(pool: &PgPool) -> sqlx::Result<Value> {
    // Unnest the generated tag_names column (already guards malformed data).
    let rows = sqlx::query(
        "select tag, count(*) as num \
         from object, unnest(tag_names) as tag \
         where library_id = $1 and kind = 'item' and not deleted \
         group by tag \
         order by tag",
    )
    .bind(LIBRARY_ID)
    .fetch_all(pool)
    .await?;
    let array = rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "tag": row.get::<String, _>("tag"),
                "numItems": row.get::<i64, _>("num"),
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

/// Delete the named settings, recording each in the deletion log so `/deleted`
/// can propagate the removal. Returns the new library version or a conflict.
pub async fn delete_settings(
    pool: &PgPool,
    keys: &[String],
    expected: Option<i64>,
) -> sqlx::Result<Outcome<i64>> {
    let mut tx = pool.begin().await?;
    let version = match guarded_version(&mut tx, expected).await? {
        Outcome::Done(version) => version,
        Outcome::Conflict(current) => return Ok(Outcome::Conflict(current)),
    };
    for key in keys {
        sqlx::query("delete from setting where library_id = $1 and key = $2")
            .bind(LIBRARY_ID)
            .bind(key)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into deletion (library_id, kind, key, version) values ($1, 'setting', $2, $3) \
             on conflict (library_id, kind, key) do update set version = $3",
        )
        .bind(LIBRARY_ID)
        .bind(key)
        .bind(version)
        .execute(&mut *tx)
        .await?;
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
