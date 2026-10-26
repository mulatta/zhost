-- Single-user library. Objects are opaque jsonb blobs keyed by (kind, key);
-- the library version counter is bumped inside each write transaction so the
-- client's version preconditions stay coherent.

create table if not exists library (
    id      bigint primary key,
    version bigint not null default 0
);
insert into library (id, version) values (1, 0) on conflict (id) do nothing;

create table if not exists object (
    library_id bigint not null references library (id),
    kind       text   not null,            -- 'item' | 'collection' | 'search'
    key        text   not null,
    version    bigint not null,
    data       jsonb  not null,
    primary key (library_id, kind, key)
);
create index if not exists object_version_idx
    on object (library_id, kind, version);

create table if not exists setting (
    library_id bigint not null references library (id),
    key        text   not null,
    version    bigint not null,
    value      jsonb  not null,
    primary key (library_id, key)
);

create table if not exists deletion (
    library_id bigint not null references library (id),
    kind       text   not null,            -- 'item'|'collection'|'search'|'setting'|'tag'
    key        text   not null,
    version    bigint not null,
    primary key (library_id, kind, key)
);
create index if not exists deletion_version_idx
    on deletion (library_id, version);
