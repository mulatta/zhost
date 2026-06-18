-- Denormalize the fields the CLI item query filters/searches/sorts on into
-- STORED generated columns. Queries then compare plain columns with standard
-- B-tree/GIN/trgm indexes instead of digging through jsonb (and unnesting arrays
-- with type guards) at query time. `data` stays the source of truth — the
-- columns are derived from it on write, so sync's verbatim round-trip is intact.
--
-- A generated expression may not contain a subquery, but may call an IMMUTABLE
-- function that does; the array extractors below guard a non-array value so
-- malformed data can neither break a column nor its index.

create or replace function zhost_tag_names(data jsonb) returns text[]
language sql immutable as $$
    select coalesce(array(
        select t->>'tag'
        from jsonb_array_elements(
            case when jsonb_typeof(data->'tags') = 'array'
                 then data->'tags' else '[]'::jsonb end) t
    ), '{}')
$$;

create or replace function zhost_collection_keys(data jsonb) returns text[]
language sql immutable as $$
    select coalesce(array(
        select c
        from jsonb_array_elements_text(
            case when jsonb_typeof(data->'collections') = 'array'
                 then data->'collections' else '[]'::jsonb end) c
    ), '{}')
$$;

alter table object
    add column item_type       text    generated always as (data->>'itemType') stored,
    add column is_top          boolean generated always as (coalesce(data->>'parentItem', '') in ('', 'false')) stored,
    add column deleted         boolean generated always as (coalesce(data->>'deleted', '0') in ('1', 'true')) stored,
    add column date_year       text    generated always as (substring(data->>'date' from '\d{4}')) stored,
    add column search_text     text    generated always as (zhost_item_text(data)) stored,
    add column tag_names       text[]  generated always as (zhost_tag_names(data)) stored,
    add column collection_keys text[]  generated always as (zhost_collection_keys(data)) stored;

-- Replace the functional search index (migration 0005) with one on the column.
drop index if exists object_item_text_trgm_idx;
create index if not exists object_search_text_trgm_idx
    on object using gin (search_text gin_trgm_ops) where kind = 'item';
create index if not exists object_item_type_idx
    on object (item_type) where kind = 'item';
create index if not exists object_tag_names_idx
    on object using gin (tag_names) where kind = 'item';
create index if not exists object_collection_keys_idx
    on object using gin (collection_keys) where kind = 'item';
