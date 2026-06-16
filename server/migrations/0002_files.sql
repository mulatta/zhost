-- Attachment file metadata. The bytes live on the filesystem (one file per
-- item key under the configured storage directory); this table tracks the
-- md5/mtime the client needs for download and change detection.
create table if not exists file (
    library_id bigint not null references library (id),
    item_key   text   not null,
    md5        text   not null,
    filename   text   not null,
    filesize   bigint not null,
    mtime      bigint not null,
    version    bigint not null,
    primary key (library_id, item_key)
);
