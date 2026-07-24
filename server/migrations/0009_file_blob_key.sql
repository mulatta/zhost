-- Point registered attachment metadata at one immutable object. Existing
-- deployments stored bytes under the item key, so preserve those paths while
-- new uploads move to candidate-specific keys.
alter table file add column blob_key text;

update file
set blob_key = case
    when library_id = 1 then item_key
    else 'libraries/' || library_id || '/' || item_key
end;

alter table file
    alter column blob_key set not null,
    add constraint file_blob_key_nonempty check (btrim(blob_key) <> '');
