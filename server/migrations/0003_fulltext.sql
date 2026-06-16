-- Extracted full-text content for attachment items, synced so each machine can
-- search a document's body without re-indexing it locally. Keyed per item; the
-- version is the library version at which the content last changed, so a
-- client's `?since` read and per-item download stay coherent.
create table if not exists fulltext (
    library_id    bigint not null references library (id),
    item_key      text   not null,
    content       text   not null,
    indexed_chars bigint not null default 0,
    total_chars   bigint not null default 0,
    indexed_pages bigint not null default 0,
    total_pages   bigint not null default 0,
    version       bigint not null,
    primary key (library_id, item_key)
);

create index if not exists fulltext_version_idx on fulltext (library_id, version);
