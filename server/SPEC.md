# zhost sync server — Zotero Web API v3 implementation spec

Derived from the Zotero client sync code (`~/git/zotero/chrome/content/zotero/xpcom/sync/`,
`storage/zfs.js`), the official Web API v3 docs, and live testing against a real
Zotero 9 client. This is the contract the server must satisfy so a
(URL-redirected) stock client syncs against it. The server implements the entire
sync contract below; "out of scope" items remain unimplemented. The
"Read/query API (CLI-facing extension)" section is also implemented, but is a
zhost addition beyond the sync subset — it is not part of what the sync client
needs.

Client file references use `syncAPIClient.js` (the request layer), `syncEngine.js`
(sync flow), `zfs.js` (file storage).

Behaviours confirmed by live testing (each is a hard requirement):

- **Auth is the login-session flow, not credentials.** Zotero 9's "Login" calls
  `POST /keys/sessions`, opens the returned `loginURL` in a browser, then polls
  `GET /keys/sessions/<token>` until it reports `status: "completed"` with the
  API key. It does not use `POST /keys` with a username/password.
- **Write bodies are gzip-compressed** (`Content-Encoding: gzip`); the server
  must decode them.
- **Write `successful` must return the full canonical object** (with `itemType`
  etc.) — echoing a partial upload makes the client throw "Unknown item type".
- **Attachment `data` must emit `linkMode` before `filename`.** The client's
  `fromJSON` processes fields in order and errors ("Link mode must be set before
  setting attachment path") otherwise; jsonb sorts keys alphabetically, so the
  server reorders linkMode to the front.
- **File download is a `302`** carrying `Location` plus `Zotero-File-MD5` and
  `Zotero-File-Modification-Time`; the client reads those headers and fetches the
  bytes from `Location` separately (it does not auto-follow the redirect).
- Attachment uploads exceed common default request-body limits; do not cap them.
- Client-facing URLs (`loginURL`, the file upload `url`, the download `Location`)
  must be the public/reverse-proxy address, not the internal bind address.

## Scope (MVP)

- **Single user, single personal library.** `userID` is configured. Groups are
  out of scope: `GET /users/<id>/groups?format=versions` returns `{}` so the
  client syncs only the personal library.
- **No S3.** File storage is local on the server host; the upload-authorization
  response points `url` at the server's own upload endpoint.
- **Out of scope:** group libraries, binary-diff file upload (`PATCH .../file` —
  an API-only capability the desktop client's `storage/zfs.js` never implements;
  it always uploads the full file, so the server never sees a partial upload),
  the streaming/websocket server (client falls back to polling), and `/schema`
  (the client uses its bundled `resource://zotero/schema/global/schema.json` and
  updates it from repo.zotero.org, not from the sync API).

## Auth

API key travels in the `Zotero-API-Key` header on every request except the key
endpoints. Reject unknown keys with 403.

### Access keys

Keys are provisioned out of band (the server never mints them) and held in
memory, loaded at boot from secret files — no key material in the store or
database. Each key carries an access level; the token bytes stay in the secret
while the level is declared in deployment config:

- **read/write** — full sync access (the Zotero app needs this).
- **read-only** — every read works, but `POST`/`PATCH`/`DELETE` are rejected
  with `403`. Intended for the CLI / an agent, so automation can't mutate the
  library.

`GET /keys/current` reports the requesting key's own access (`access.user.write`
reflects read-only); the login session (`/keys/sessions`) hands out a read/write
key, since the app that drives it needs to write. Config: `ZHOST_KEYS` is a
comma-separated list of `<role>:<path>` entries (`rw`/`ro`); each path is a
single-line token file. A single `ZHOST_API_KEY_FILE`/`ZHOST_API_KEY` is still
accepted as one read/write key.

| Endpoint | Notes |
|---|---|
| `GET /keys/current` | Return `{userID, username, displayName, access}` with full access (`access.user = {library:true, notes:true, write:true, files:true}`). Client reads `userID` + `access`. (syncAPIClient.js:53) |
| `POST /keys/sessions` | **The login flow Zotero 9 actually uses.** Respond `201` `{sessionToken, loginURL}`. (syncAPIClient.js:575) |
| `GET /keys/sessions/<token>` | Polled until done; single user, so answer immediately with `200` `{status:"completed", apiKey, userID, username}`. |
| `POST /keys` | Legacy credentials→key. Not used by Zotero 9; the key is provisioned out of band (a secret file) instead. (syncAPIClient.js:535) |

## Required headers

**Request (all):** `Zotero-API-Version` (int, e.g. 3), `Zotero-Schema-Version`
(int), `Zotero-API-Key`. **Write requests also:** `If-Unmodified-Since-Version`,
`Content-Type: application/json` (or `application/x-www-form-urlencoded` for file
params), and `Content-Encoding: gzip` — the JSON body is gzipped and must be
decoded before parsing.

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
| `GET <prefix>/items/<key>/fulltext` | `200` `{indexedChars, totalChars, indexedPages, totalPages, content}` or `404`. The `Last-Modified-Version` here **must equal** that item's value in the versions map above (it is the row's own version, not the current library version); otherwise the client re-downloads content it already holds every sync. (syncAPIClient.js:484, syncFullTextEngine.js:91) |

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

**Upload-result object** (HTTP 200, parsed at syncEngine.js:1375). Keyed by the
object's index in the submitted batch; each value is the **full canonical
object** the client round-trips (a partial echo is rejected):
```json
{
  "successful": { "0": { "key": "...", "version": N, "data": { } } },
  "success":    {},
  "unchanged":  {},
  "failed":     {}
}
```
`failed` code `<500` = permanent (client drops), `>=500` = retry.

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
   `204` + `Last-Modified-Version`; commit the file and **stamp `md5`/`mtime`
   into the attachment item's `data`** (the downloading client needs them to
   reconcile the file), bumping the library version. (zfs.js:703)

**Download** — `GET <prefix>/items/<key>/file`. Respond **`302`** (the client
reads metadata from it without following) with `Location` pointing at a bytes
endpoint, plus `Zotero-File-MD5`, `Zotero-File-Modification-Time` and
`Zotero-File-Compressed: No`. The client then GETs `Location` for the bytes.
`404` if absent. (zfs.js:52, zfs.js:109)

## Pagination & throttling

Multi-object listings page via `Link: <next>; rel=next` and `Total-Results`;
client follows `rel=next` until absent (syncAPIClient.js:923). MVP can return all
results in one page (no `Link`). `Backoff`/`Retry-After` honored by the client
but optional to emit.

## Read/query API (CLI-facing extension)

Beyond the sync subset above, these read endpoints exist so a token-holding CLI
or agent can find and fetch items without the Zotero app. They are query-only
(no styled bibliography/citation rendering, no schema endpoints — those stay
downstream, e.g. an agent or pandoc working from the raw JSON / CSL-JSON). A
read-only key (see Auth) is enough to drive all of them, so automation can read
without write access.

Item listings (`GET /users/<id>/items?format=json`) accept:

| Param | Meaning |
|---|---|
| `q` + `qmode` | Search term. `titleCreatorYear` (default) matches title/creators/date; `everything` also matches stored full-text content. |
| `itemType` | Filter by type (`book`; OR via `a \|\| b`; negate via `-note`). |
| `tag` | Filter by tag (repeat for AND, `\|\|` for OR). |
| `sort` + `direction` | `dateModified` (default), `dateAdded`, `title`, `creator`, `date`, `itemType`; `asc`/`desc`. |
| `limit` + `start` | Page window (default 25, max 100). Response sets `Total-Results` and `Link: …; rel="next"`. |
| `includeTrashed` | Include trashed items (excluded by default). |

Convenience listings: `GET /users/<id>/items/top` (no `parentItem`),
`/items/trash` (`data.deleted`), `/tags` (distinct tags as `[{tag, numItems}]`,
trashed items excluded), `/collections/<key>/items` (`data.collections` contains
the key). The item listings carry the same search/filter/sort/paginate params as
`/items`; `/items/top` also still answers the sync `format=versions`/`itemKey`
reads the client may send there.

Matching is case-insensitive substring (`ILIKE`). The default `titleCreatorYear`
search runs against an immutable `zhost_item_text(data)` expression (title, date
and creator names) carrying a `pg_trgm` GIN index (migration `0005`), so it is
index-backed rather than a sequential scan. Full-text search (`qmode=everything`)
additionally matches the `fulltext` table — also `pg_trgm`-indexed on `content`
(migration `0004`); because that mode ORs across the `fulltext` table it falls
back to a scan of the candidate items.

## Storage backend (PostgreSQL on malt)

Tables (see `migrations/`):
- `library(id, version)` — the monotonic counter.
- `object(library_id, kind, key, version, data jsonb)` — items, collections,
  searches; `version`-indexed for `since` queries. Trashed state lives in
  `data.deleted`, not a column.
- `setting(library_id, key, value jsonb, version)`.
- `fulltext(library_id, item_key, content, indexed_chars, total_chars, indexed_pages, total_pages, version)`.
- `deletion(library_id, kind, key, version)` — the delete log for `/deleted`.
- `file(library_id, item_key, md5, filename, filesize, mtime, version)` — the
  bytes live on the filesystem (one file per item key under `storageDir`).

Not in the database: **API keys** are held in memory, loaded from secret files
at boot (see Auth → Access keys). **Pending uploads** (between the file
authorisation and registration steps) are held in memory keyed by upload key.

Writes run in a transaction that bumps `library.version` and stamps the affected
rows, so conditional version checks are atomic.

## Resolved by live testing

- `data` JSON shape: stored opaquely and round-tripped verbatim; only attachment
  field order matters (linkMode first, see above).
- `Zotero-Write-Token` is not required — the client never sends it; the
  `If-Unmodified-Since-Version` → 412 guard is sufficient.
- `mtime` is in milliseconds.
- `POST /keys` is unused; the key is provisioned as a secret file and the client
  obtains it through the login session.
