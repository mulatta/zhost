-- Zotero uploads multi-file attachments as ZIP objects while retaining the
-- main file's hash in item metadata. Track both integrity domains so the
-- candidate bytes can be verified without replacing the client-visible hash.

alter table file
    add column blob_md5 text,
    add column compressed boolean not null default false;

update file set blob_md5 = md5;

alter table file
    alter column blob_md5 set not null;

alter table pending_uploads
    add column upload_md5 text,
    add column compressed boolean not null default false;

update pending_uploads set upload_md5 = md5;

alter table pending_uploads
    alter column upload_md5 set not null;
