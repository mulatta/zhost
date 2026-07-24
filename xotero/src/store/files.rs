use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use crate::domain::{GroupId, LibraryId, UserId};

use super::is_stored_file_attachment;

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
