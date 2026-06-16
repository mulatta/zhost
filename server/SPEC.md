# zhost sync server — Zotero Web API v3 implementation spec

Derived from the Zotero client sync code (`~/git/zotero/chrome/content/zotero/xpcom/sync/`,
`storage/zfs.js`) and the official Web API v3 docs. This is the contract the
server must satisfy so a (URL-redirected) stock Zotero client syncs against it.

Client file references use `syncAPIClient.js` (the request layer), `syncEngine.js`
(sync flow), `zfs.js` (file storage).

## Scope (MVP)

- **Single user, single personal library.** `userID` is configured. Groups are
  out of scope: `GET /users/<id>/groups?format=versions` returns `{}` so the
  client syncs only the personal library.
- **No S3.** File storage is local on the server host; the upload-authorization
  response points `url` at the server's own upload endpoint.
- **Out of scope:** group libraries, binary-diff file upload (`PATCH .../file`),
  the streaming/websocket server (client falls back to polling), and `/schema`
  (the client uses its bundled `resource://zotero/schema/global/schema.json` and
  updates it from repo.zotero.org, not from the sync API).

## Auth

API key travels in the `Zotero-API-Key` header on every request except the key
endpoints. MVP accepts one configured key; reject others with 403.

| Endpoint | Notes |
|---|---|
| `GET /keys/current` | Return `{userID, username, displayName, access}` with full access (`access.user = {library:true, notes:true, write:true, files:true}`). Client reads `userID` + `access`. (syncAPIClient.js:53) |
| `POST /keys` | Credentials→key. Body `{username, password, name, access}`; respond `201` `{key, userID, username, displayName, access}`. MVP may return the single configured key regardless of credentials, or skip if the key is provisioned manually. (syncAPIClient.js:535) |
| `POST /keys/sessions`, `GET /keys/sessions/<token>` | OAuth-style login session. Optional — only needed if supporting in-app login flow. (syncAPIClient.js:575) |

## Required headers

**Request (all):** `Zotero-API-Version` (int, e.g. 3), `Zotero-Schema-Version`
(int), `Zotero-API-Key`. **Write requests also:** `If-Unmodified-Since-Version`,
`Content-Type: application/json` (or `application/x-www-form-urlencoded` for file
params).

**Response (server must set):** `Last-Modified-Version` on every data response and
version-listing response. `Link` (`<url>; rel=next`) + `Total-Results` for
pagination. Optional `Backoff` / `Retry-After` (MVP can omit — no throttling).

## Version model

A monotonic **library version** counter. Every write that changes the library
bumps it; the new value is returned in `Last-Modified-Version`. Each object also
carries its own `version` (the library version at which it last changed).

- **Reads** with `since=<v>` (and `If-Modified-Since-Version: <v>`): return only
  objects with `version > v`; `304 Not Modified` if nothing changed.
- **Writes** send `If-Unmodified-Since-Version: <clientLibraryVersion>`. If the
  server's library version is greater → `412 Precondition Failed`. The client
  then restarts the sync from the top. (syncAPIClient.js:1019)
- Writes must be serialized per library so the counter and conditional checks
  are atomic.

## Data sync endpoints

`<prefix>` = `/users/<userID>`. Objects: `collections`, `searches`, `items`
(also `settings`, `tags` handled specially).

### Download

| Request | Response |
|---|---|
| `GET <prefix>/settings?since=<v>` (`If-Modified-Since-Version`) | `200` `{settingKey: {value: ...}}` or `304`. (syncAPIClient.js:118) |
| `GET <prefix>/deleted?since=<v>` | `200` `{collections:[keys], searches:[keys], items:[keys], tags:[{tag,type}]}`. `409` if `since` precedes the delete-log start. (syncAPIClient.js:151) |
| `GET <prefix>/{collections,searches,items}?format=versions&since=<v>` (items also `includeTrashed=1`, optional `?top` via `items/top`) | `200` `{key: version}` or `304`. (syncAPIClient.js:213) |
| `GET <prefix>/{collections,searches,items}?{objectKey}=k1,k2,...&format=json` | `200` array of `{key, version, data}`. Client batches ≤100 keys/request. (syncAPIClient.js:269) |
| `GET <prefix>/fulltext?format=versions&since=<v>` | `200` `{itemKey: version}`. (syncAPIClient.js:462) |
| `GET <prefix>/items/<key>/fulltext` | `200` `{indexedChars, totalChars, indexedPages, totalPages, content}` or `404`. (syncAPIClient.js:484) |

### Write

All writes return `Last-Modified-Version` and use `If-Unmodified-Since-Version`.

| Request | Response |
|---|---|
| `POST <prefix>/{collections,searches,items}` — body: JSON array (client batches ≤10, API allows ≤50) | `200` upload-result object (below) or `412`. (syncAPIClient.js:393) |
| `PATCH <prefix>/{...}` | Same as POST (updates). |
| `DELETE <prefix>/{collections,searches,items}?{objectKey}=k1,k2,...` (client batches ≤25); tags via `?tags=t1\|\|t2` | `204` or `412`. (syncAPIClient.js:430) |
| `POST <prefix>/settings` — body `{key: {value}}` (≤250/batch) | `200` result obj / `204` / `412`. (syncAPIClient.js:359) |
| `DELETE <prefix>/settings?settingKey=k1,k2` | `204` / `412`. (syncAPIClient.js:336) |
| `POST <prefix>/fulltext` — array `{key, content, indexedChars, totalChars, indexedPages, totalPages}` (≤10 items / 500KB) | `200` result obj / `412`. (syncAPIClient.js:506) |

**Upload-result object** (HTTP 200, parsed at syncEngine.js:1375):
```json
{
  "successful": [ { "key": "...", "version": N, "data": { } } ],
  "unchanged":  [ "key", ... ],
  "failed":     [ { "key": "...", "code": 400, "message": "..." } ]
}
```
`failed` code `<500` = permanent (client drops), `>=500` = retry. New objects
without a key are assigned a server key and returned in `successful`.

## File sync (ZFS-style, local storage)

Three-step upload, then download. Client code: `zfs.js`.

1. **Authorization** — `POST <prefix>/items/<key>/file`,
   `Content-Type: application/x-www-form-urlencoded`, header `If-None-Match: *`
   (new) or `If-Match: <oldmd5>` (replace). Body:
   `md5, filename, filesize, mtime` (mtime in ms; multi-file attachments add
   `zipMD5, zipFilename`). Responses:
   - already present → `200` `{ "exists": 1 }` (+ `Last-Modified-Version`).
   - upload needed → `200` `{ url, contentType, prefix, suffix, uploadKey }`
     (point `url` at the server's own upload endpoint; `prefix`/`suffix` wrap the
     body; MVP can use empty prefix/suffix + a direct PUT-like POST).
   - `412` changed remotely, `413` over quota (+ `Zotero-Storage-Usage/Quota`),
     `428` missing precondition header. (zfs.js:326)
2. **Upload** — `POST` to `url` with body `prefix + filebytes + suffix`,
   `Content-Type: contentType`. Respond `201`. (zfs.js:604) Server stores bytes
   keyed by `uploadKey`.
3. **Registration** — `POST <prefix>/items/<key>/file`,
   form body `upload=<uploadKey>`, same `If-Match`/`If-None-Match`. Respond
   `204` + `Last-Modified-Version`; commit the file, update attachment
   `md5`/`mtime`, bump library version. (zfs.js:703)

**Download** — `GET <prefix>/items/<key>/file`. Return the bytes (200) or a
`302` to a location; set `ETag`/`Zotero-File-MD5` to the md5,
`Zotero-File-Modification-Time` to mtime. `404` if absent. (zfs.js:52)

## Pagination & throttling

Multi-object listings page via `Link: <next>; rel=next` and `Total-Results`;
client follows `rel=next` until absent (syncAPIClient.js:923). MVP can return all
results in one page (no `Link`). `Backoff`/`Retry-After` honored by the client
but optional to emit.

## Storage backend (PostgreSQL on malt)

Sketch:
- `library(id, version)` — the monotonic counter.
- `object(library_id, type, key, version, data jsonb, deleted bool)` — items,
  collections, searches; `version`-indexed for `since` queries.
- `setting(library_id, key, value jsonb, version)`.
- `fulltext(library_id, item_key, content, indexed_chars, total_chars, indexed_pages, total_pages, version)`.
- `deletion(library_id, type, key, version)` — the delete log for `/deleted`.
- `file(library_id, item_key, md5, filename, filesize, mtime, blob_path, version)`.
- `upload(upload_key, library_id, item_key, tmp_path, md5, ...)` — pending uploads.
- `api_key(key, user_id, access)` — MVP one row.

Writes run in a transaction that bumps `library.version` and stamps the affected
rows, so conditional version checks are atomic.

## Open questions / verify against a live sync

- Exact `data` JSON shape per object type (mirror what `format=json` download
  returns; the client round-trips it).
- Whether the client requires `Zotero-Write-Token` (idempotency) — not observed
  in the read pass; confirm during write testing.
- `mtime` units: file docs say ms; confirm client `zfs.js` sends ms.
- `POST /keys` necessity vs. provisioning the key out of band.
