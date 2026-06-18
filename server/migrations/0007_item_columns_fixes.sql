-- Fix two generated columns from migration 0006. Both columns are dropped and
-- re-added so the stored values are recomputed with the corrected expressions.

-- date_year: anchor the 4-digit year to word boundaries so a longer number like
-- "12345" yields no year instead of a bogus "1234".
alter table object drop column date_year;
alter table object
    add column date_year text generated always as (substring(data->>'date' from '\m\d{4}\M')) stored;

-- zhost_tag_names: skip non-object array elements and missing tags, so a
-- malformed tags array (e.g. ["x"] or [123]) can't put a NULL into tag_names and
-- crash the /tags listing (unnest of a NULL element).
create or replace function zhost_tag_names(data jsonb) returns text[]
language sql immutable as $$
    select coalesce(array(
        select t->>'tag'
        from jsonb_array_elements(
            case when jsonb_typeof(data->'tags') = 'array'
                 then data->'tags' else '[]'::jsonb end) t
        where jsonb_typeof(t) = 'object' and t->>'tag' is not null
    ), '{}')
$$;
-- Re-add the column so it recomputes with the corrected function, and rebuild
-- its index (dropping the column dropped it).
alter table object drop column tag_names;
alter table object
    add column tag_names text[] generated always as (zhost_tag_names(data)) stored;
create index if not exists object_tag_names_idx
    on object using gin (tag_names) where kind = 'item';
