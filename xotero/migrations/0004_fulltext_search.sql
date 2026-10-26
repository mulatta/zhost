-- Make stored full-text content searchable for the CLI-facing query API
-- (`GET /items?q=…&qmode=everything`). A trigram GIN index keeps the
-- case-insensitive substring match (`content ILIKE '%term%'`) cheap as the
-- corpus grows. pg_trgm is a trusted extension since PostgreSQL 13, so the
-- database owner can create it without superuser rights.
create extension if not exists pg_trgm;

create index if not exists fulltext_content_trgm_idx
    on fulltext using gin (content gin_trgm_ops);
