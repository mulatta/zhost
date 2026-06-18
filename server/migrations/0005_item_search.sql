-- Make the CLI item search (q with the default titleCreatorYear qmode) index-
-- backed at scale. Title/date/creator search ran as a sequential scan over the
-- object table because, unlike fulltext.content (migration 0004), the items had
-- no trigram index. Fold the searched fields into one immutable expression and
-- give it a trigram GIN index.
--
-- The function is IMMUTABLE so it can index an expression, and guards a non-array
-- `creators` value (data is stored opaquely) with jsonb_typeof so a malformed
-- item can neither break the index nor the query.
create or replace function zhost_item_text(data jsonb) returns text
language sql immutable as $$
    select concat_ws(' ',
        data->>'title',
        data->>'date',
        (select string_agg(
                    concat_ws(' ', c.value->>'lastName', c.value->>'firstName', c.value->>'name'),
                    ' ' order by c.ordinality)
         from jsonb_array_elements(
                  case when jsonb_typeof(data->'creators') = 'array'
                       then data->'creators' else '[]'::jsonb end)
              with ordinality as c(value, ordinality)))
$$;

create index if not exists object_item_text_trgm_idx
    on object using gin (zhost_item_text(data) gin_trgm_ops)
    where kind = 'item';
