-- Durable attachment upload capabilities. Tokens are stored only as SHA-256
-- digests; candidate bytes remain immutable in object storage until one
-- registration transaction makes their pointer live.

create table pending_uploads (
    token_hash            bytea       primary key,
    library_id            bigint      not null references library (id) on delete restrict,
    group_id              bigint      references groups (id) on delete restrict,
    authorizer_key_hash   bytea       not null,
    bootstrap_user_id     bigint      references users (id) on delete restrict,
    item_key              text        not null,
    expected_md5          text,
    blob_key              text        not null unique,
    md5                   text        not null,
    filename              text        not null,
    filesize              bigint      not null,
    mtime                 bigint      not null,
    state                 text        not null default 'authorized',
    gc_attempted_at       timestamptz,
    created_at            timestamptz not null default now(),
    expires_at            timestamptz not null default (now() + interval '1 hour'),
    constraint pending_uploads_token_hash_length
        check (octet_length(token_hash) = 32),
    constraint pending_uploads_authorizer_key_hash_length
        check (octet_length(authorizer_key_hash) = 32),
    constraint pending_uploads_item_key_nonempty
        check (btrim(item_key) <> ''),
    constraint pending_uploads_blob_key_nonempty
        check (btrim(blob_key) <> ''),
    constraint pending_uploads_filesize_nonnegative
        check (filesize >= 0),
    constraint pending_uploads_state_valid
        check (state in ('authorized', 'uploading', 'uploaded', 'discarded', 'deleting')),
    constraint pending_uploads_expiry_valid
        check (expires_at > created_at)
);

create index pending_uploads_expires_at_idx on pending_uploads (expires_at);
