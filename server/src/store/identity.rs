use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use crate::domain::{ApiKeyId, LibraryId, Permissions, Principal, UserId};

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
