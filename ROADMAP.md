# zhost roadmap

zhost aims to become a self-hosted, multi-user Zotero-compatible server and a
foundation for dedicated research clients. The server follows the public Zotero
API contract while keeping its storage and deployment architecture smaller than
Zotero's production `dataserver` stack.

This roadmap separates protocol compatibility from implementation parity:
`dataserver` is a behavioral reference, not an architecture to reproduce.

## Product direction

The project will develop in this order:

1. Inventory and specify the upstream Zotero API layers.
2. Build the zhost server first, including multi-user, groups, roles, and
   library-scoped permissions.
3. Evolve and optimize PostgreSQL storage through explicit migrations.
4. Use URL-patched upstream Zotero desktop releases throughout pre-alpha.
5. Begin a dedicated agent-first client after the server reaches alpha.
6. Enter beta after the dedicated client is stable for one user, then harden
   multi-user operation.
7. Begin a custom browser Web Library after the beta server and client stabilize.
8. Use release candidates to stabilize the server, dedicated GUI, and browser
   Web Library together.

## Compatibility promise

The final target is the complete **public** Zotero API, delivered in versioned
compatibility profiles. Internal Zotero.org operations and production
infrastructure are not part of that promise.

A supported endpoint must match upstream behavior in:

- HTTP method and path;
- authentication and authorization;
- request and response JSON;
- required and exposed headers;
- pagination and filtering;
- library and object version semantics;
- conditional reads and writes;
- group and role permissions;
- file registration, upload, and download;
- error status and retry behavior;
- documented output formats.

Partial endpoint compatibility is not sufficient. Unsupported endpoints and
formats must fail explicitly rather than return a plausible but incompatible
shape.

### Sources of truth

Compatibility work uses these sources, in order, with conflicts recorded:

1. Versioned public Zotero Web API documentation
2. Observable behavior of supported official Zotero desktop releases
3. Upstream `zotero/dataserver` controllers, models, and tests
4. Requests made by upstream `zotero/web-library`
5. Existing zhost behavior, only where it does not conflict with upstream

The `dataserver` source defines many edge cases not described in public docs. Its
MySQL schema, sharding, caches, queues, billing, and operational services do not
constrain zhost internals.

## API compatibility profiles

### Profile A: Sync and Data Core

Required before alpha:

- API keys and current-key introspection
- Browser-approved desktop login sessions
- Users and personal libraries
- Groups, memberships, and upstream role semantics
- Items, collections, saved searches, tags, and settings
- Deleted-object feeds
- Full-text synchronization
- Attachment registration, upload, replacement, and download
- Library and object versions
- Conditional reads and writes
- Pagination, sorting, filtering, and documented query behavior
- Personal and group library route families
- Read, write, notes, and file permissions

Profile A is sufficient for official desktop synchronization and core Web API
consumers. It must support multiple users and private groups before alpha, even
though multi-user production hardening continues during beta.

### Profile B: Public API Completeness

Implemented incrementally during alpha and beta, and required for the eventual
public-API-complete release:

- Single-object and child-object endpoints
- Trash and publications behavior
- Public user and group access where documented
- Group metadata and membership management APIs
- All documented item and collection query variants
- Documented export representations and content negotiation
- Citation and bibliography output formats
- Remaining public key and permission management behavior
- Other endpoints confirmed as part of the public Zotero API inventory

The inventory, not this summary, becomes the authoritative checklist.

### Profile C: zhost ecosystem APIs

zhost-specific APIs use a separate namespace and never alter upstream paths:

```text
/zhost/capabilities
/zhost/admin/...
/zhost/agents/...
```

Potential Profile C features:

- Instance administration
- Agent-safe search and write workflows
- Device and agent pairing
- Health, readiness, and diagnostics
- Client capability discovery

### Excluded from the public compatibility promise

Unless later promoted through a separate proposal:

- Zotero.org internal administration
- Storage billing and commercial quota accounting
- MySQL sharding and replica management
- Redis and Memcached topology
- Elasticsearch operations
- SQS, SNS, StatsD, and Scribe infrastructure
- TTS services
- Translation and document-recognition services
- Zotero.org website, forums, profiles, and account-recovery implementation

These exclusions do not remove public Web API behavior merely because upstream
implements that behavior using one of these systems. zhost must provide a
smaller implementation when a public endpoint depends on it.

## Deployment and tenancy model

One zhost deployment is one tenant. A deployment contains multiple users,
personal libraries, and group libraries.

No application-level `tenant_id` is planned. Deployment-level isolation comes
from separate zhost, PostgreSQL, object-storage, and identity-provider
configuration. Managed multi-tenant hosting can run isolated deployments rather
than weakening the internal authorization model with a second tenancy layer.

## Identity and authentication

### Identity provider boundary

OIDC remains the responsibility of a trusted reverse proxy such as
`oauth2-proxy`. zhost maps verified external identity claims to local users.
Provider identity is keyed by stable `(issuer, subject)`, not email.

```text
OIDC provider
    -> trusted reverse proxy
    -> verified issuer/subject claims
    -> zhost local user
    -> user-owned API key
    -> authorized library context
```

The backend must bind only to a trusted interface or socket. The proxy must
remove caller-supplied identity headers before forwarding verified claims.

### Zotero API credentials

Zotero clients continue to authenticate with `Zotero-API-Key`. API keys become
DB-backed, user-owned, scoped, revocable credentials. Static credential-file
keys remain available for bootstrap, recovery, and simple development installs.

Desktop login sessions must:

1. start without a released key;
2. authenticate a browser through the OIDC edge;
3. map that browser identity to one local user;
4. require explicit approval;
5. create or select a user-owned key;
6. return only that key to the polling desktop client.

No shared read/write key may be returned to multiple users.

## Authorization model

Every authenticated request resolves to one explicit context equivalent to:

```rust
struct RequestContext {
    principal: Principal,
    library: LibraryId,
    permissions: Permissions,
}
```

Personal and group routes share data handlers after target resolution:

```text
/users/<id>/...   -> personal library -> RequestContext
/groups/<id>/...  -> group library    -> RequestContext
```

Initial upstream group roles:

- owner
- admin
- member

The API inventory must capture the exact upstream effects of:

- group type and visibility;
- library reading policy;
- library editing policy;
- file editing policy;
- owner-only and admin-only operations;
- API-key-specific permissions.

URL identifiers, object keys, and file keys must never bypass this context.

## Data model and migration policy

### Canonical Zotero objects

Items, collections, searches, and flexible settings remain canonical JSONB
objects. This preserves unknown upstream fields and supports object-level sync
without reproducing every normalized `dataserver` table.

Indexed projections may be added for query performance:

```text
item_type
parent_key
deleted
creator_names
tag_names
collection_keys
search_text
sort fields
```

A projection must be derivable from canonical data or have a documented rebuild
path.

### Relational security and ownership data

Security invariants are not deferred optimizations. The pre-alpha multi-user
foundation requires relational tables equivalent to:

```text
users
external_identities
api_keys
libraries
personal_libraries
groups
group_memberships
login_sessions
```

Exact table names may change during pre-alpha. Foreign keys, unique constraints,
and transaction boundaries must enforce identity, ownership, membership, and
library isolation.

### Files

Object-storage keys must include library scope:

```text
libraries/<library-id>/items/<item-key>
```

Legacy bare item keys from the current single-user implementation require an
explicit transition:

- read namespaced keys first;
- temporarily fall back to legacy keys for the original library;
- write only namespaced keys;
- provide auditable migration and cleanup tooling;
- remove fallback only in a documented breaking release.

### Migration rules

- Every schema change is an ordered migration.
- Migration tests start from realistic previous schemas and data.
- Pre-alpha may contain breaking migrations, but breakage must be declared.
- Alpha introduces documented upgrade paths and pre-upgrade backup guidance.
- Beta migrations preserve user data across supported releases.
- Release candidates test backup, migration, restore, and rollback-by-restore.
- Performance normalization happens only from measured query or integrity needs.

## Agent architecture

The dedicated client is agent-first but does not own provider credentials. Agent
subscriptions remain inside native agent runtimes.

Agent integration has two independent levels.

### Tool adapters

Required and broadly portable:

```text
Agent runtime -> zhost tool adapter -> zhost API
```

Initial safe tools:

- search library objects;
- retrieve item metadata;
- retrieve attachment text;
- list collections and related items;
- draft and create notes;
- propose and apply tags;
- create item relations;
- render citations when supported.

Read operations may be automatic. Writes require scoped credentials and an
explicit approval policy. Coding-agent filesystem and shell tools are not
implicitly exposed by the research client.

Common implementations may include:

- a machine-readable `zhostctl` CLI;
- Pi extension tools;
- Claude Code plugin/MCP/skill adapters;
- Codex adapters;
- OpenClaw/OpenCode adapters.

### Session adapters

Optional and capability-driven:

```text
Dedicated GUI -> session adapter -> controllable agent runtime
```

Potential capabilities:

```text
create/resume session
prompt
stream output
steer/follow-up
abort
tool events
approval requests
branching
```

Pi is the first deep session adapter because it provides an SDK and strict JSONL
RPC mode while using its own stored subscription or API credentials. Agents that
do not expose a stable remote-control surface still receive tool adapters and
run in their native UI. Claude Code is not required to serve as an embedded GUI
backend.

The GUI negotiates adapter capabilities rather than pretending all agents have
the same control surface.

## Client strategy

### Pre-alpha client

Use supported upstream Zotero desktop releases with only endpoint/default
configuration patches. No desktop fork is planned during pre-alpha.

This client is the compatibility oracle for Profile A. Each supported release is
pinned and exercised by end-to-end tests.

### Dedicated agent-first client

Development begins at alpha. It is a custom client over the public Zotero API and
zhost ecosystem APIs, not a requirement for server operation.

Initial development order:

1. Shared typed Zotero API client
2. Library browsing and search
3. Item, collection, tag, and note editing
4. Attachment and PDF workflows
5. Agent tool protocol
6. Pi tool and session adapters
7. Agent-assisted note, comparison, and citation workflows
8. Local desktop shell and credential handling

The client must remain useful when no agent is installed or connected.

### Custom browser Web Library

Development begins only after a stable beta. It may share API, domain, editor,
and UI packages with the dedicated client, but browser security, storage CORS,
OIDC sessions, offline behavior, and remote agent connectivity remain explicit
browser concerns.

Upstream `zotero/web-library` remains a behavioral and component reference.
Whether to reuse selected upstream packages or implement client surfaces anew is
decided after the shared API/client architecture is proven. Copying ad hoc source
without an upstream-update strategy is not acceptable.

## Release train

## Pre-alpha: server compatibility construction

Pre-alpha is for rapid server development and may break schema and configuration.
It is not a production-security claim.

### Deliverables

- Complete public API inventory and compatibility profiles
- Request/response fixtures from official clients and `dataserver`
- Multi-user relational identity foundation
- OIDC edge identity mapping
- User-owned and scoped API keys
- Personal libraries
- Private groups, memberships, and roles
- Profile A user and group endpoints
- Library-scoped PostgreSQL operations
- Library-scoped S3 operations
- URL-patched official Zotero desktop package
- Desktop multi-user and group sync tests
- Initial backup and migration tooling

### Exit criteria for alpha

- Profile A inventory is complete and implemented.
- Two users have isolated personal libraries.
- Two users can synchronize one private group library.
- Owner/admin/member behavior matches captured upstream contracts.
- Metadata, files, full text, conflicts, and deletions pass desktop E2E tests.
- Cross-user and cross-library access tests fail closed.
- No production code uses a global library ID or shared login key.
- A documented migration exists from the current single-user schema and storage.

## Alpha: API stabilization and dedicated client construction

Alpha starts when Profile A server behavior works end to end. APIs may still
change, but changes require compatibility notes and migrations.

### Deliverables

- Versioned Profile A contract suite
- Progressive Profile B implementation
- Stable typed client SDK
- Agent tool protocol and `zhostctl`
- Pi tool adapter
- Pi SDK/RPC session-adapter prototype
- Dedicated GUI implementation
- Health/readiness endpoints
- Structured and secret-safe logging
- Documented backup, restore, and key rotation
- NixOS deployment and initial OCI/Compose paths

### Exit criteria for beta

- Dedicated GUI is stable for one user and one personal library.
- Core non-agent library workflows function without Pi or another agent.
- Pi subscription credentials remain owned by Pi and never enter zhost or GUI
  storage.
- Desktop and dedicated clients pass the same server contract suite.
- Supported alpha upgrades preserve metadata and files.
- Remaining multi-user risks have explicit beta hardening tests.

## Beta: multi-user hardening

Beta treats existing multi-user functionality as a production-bound security and
concurrency surface.

### Deliverables

- OIDC provisioning and account-linking hardening
- API-key creation, revocation, rotation, and audit behavior
- Group membership and role-transition hardening
- Concurrent write and version-conflict tests
- Cross-library object/file/presigned-URL isolation tests
- Restart-safe login and enrollment behavior
- Abuse limits and bounded resource usage
- Larger realistic data and group workloads
- Backup/restore drills across multiple users and groups
- Continued Profile B completion
- Second agent tool adapter to validate portability

### Stable-beta gate

- No known cross-user or cross-library isolation failure.
- Role matrix and key-permission matrix have complete integration coverage.
- Multi-device and multi-user conflict behavior matches upstream.
- Migration and restore tests cover all supported beta releases.
- One deployment can be operated without undocumented manual database changes.

After this gate, custom browser Web Library development may begin.

## Release candidates: client convergence

Release candidates freeze declared API profiles and stabilize all supported
client surfaces.

### Deliverables

- Dedicated GUI stabilization
- Custom browser Web Library stabilization
- Shared UI/domain package hardening where applicable
- Browser OIDC/session and attachment CORS hardening
- Desktop, GUI, and browser compatibility matrix
- Accessibility and localization pass
- Installer, NixOS, OCI, and Compose release artifacts
- Upgrade, backup, restore, and disaster-recovery documentation
- Security review and release threat model

### Exit criteria for 1.0

- Declared compatibility profiles are complete and versioned.
- Official patched Zotero, dedicated GUI, and custom Web Library pass release E2E
  suites.
- Supported upgrades preserve all user, group, metadata, full-text, and file
  data.
- Authentication, authorization, and browser boundaries have no known critical
  defects.
- Deployment and recovery can be reproduced from public documentation.

## Testing strategy

### API contract tests

Each inventory entry links to:

- upstream documentation;
- `dataserver` implementation location;
- official-client request fixture;
- expected response fixture;
- zhost integration test;
- profile and support status.

### Role and isolation tests

Use real database and object-storage services. Cover:

```text
user A personal -> user B denied
user A group owner -> full group control
user B group admin -> upstream admin permissions
user C group member -> configured read/write/file permissions
removed member -> immediate access loss
revoked key -> immediate access loss
same item key in two libraries -> isolated metadata and bytes
```

### Client E2E tests

- Official Zotero desktop remains mandatory through all phases.
- Dedicated GUI joins the gate during alpha.
- Browser Web Library joins after stable beta.
- Agent adapters are tested separately from core library workflows.

## Immediate work plan

Current pre-alpha focus is GUI compatibility closure for the multi-user server
slice already implemented on `zhost-multi-user-support`. Code structure work is
complete enough for this phase: Nix files live under `nix/`, Rust has a root
Cargo workspace, HTTP concerns are split from bootstrap, and store concerns are
split behind the `store` facade.

1. Run the temporary patched Zotero GUI harness only, starting from conflict
   behavior.
2. Finish metadata, attachment, full-text, network retry, restart, deletion, and
   final profile-integrity audit checks.
3. Convert every novel GUI-observed request contract or failure mode into a
   NixOS integration regression before changing production code.
4. Keep `cargo fmt`, `cargo clippy --all-targets --all-features`, `cargo test`,
   and the `nixos-sync` / `nixos-key-auth` checks green after each atomic code
   change.
5. Preserve normal Zotero profile/data by using only the recorded temporary app,
   profile, data directory, and pueue task IDs.
6. After GUI compatibility closure, resume API inventory work: create
   `docs/api/`, enumerate public and `dataserver` routes, map zhost support,
   and publish Profile A compatibility status.
7. Continue hardening owner/admin/member transitions, revocation, backup,
   restore, bounded upload memory, and migration behavior for alpha exit.

## Decision log

| Decision                                                            | Status   | Reason                                                                |
| ------------------------------------------------------------------- | -------- | --------------------------------------------------------------------- |
| Public Zotero API completeness is the final server target           | Accepted | Enables standard clients and prevents product-specific protocol drift |
| Deliver API compatibility through profiles                          | Accepted | Keeps the large public surface measurable and releasable              |
| Use `dataserver` as behavior reference, not deployment architecture | Accepted | Preserves semantics without MySQL/shard/cache infrastructure          |
| One deployment equals one tenant                                    | Accepted | Multi-user/groups need no second tenancy layer                        |
| Implement multi-user/groups/roles during pre-alpha                  | Accepted | They are foundational API semantics, not optional GUI features        |
| Keep flexible Zotero objects canonical in JSONB                     | Accepted | Supports object-level sync and upstream schema evolution              |
| Normalize identities, keys, libraries, groups, and memberships      | Accepted | Security invariants require relational constraints                    |
| Authenticate OIDC at a trusted reverse proxy                        | Accepted | Keeps provider-specific protocol outside zhost                        |
| Map `(issuer, subject)` to local users                              | Accepted | Email is not a stable identity key                                    |
| Use patched official Zotero throughout pre-alpha                    | Accepted | Provides a mature client and compatibility oracle                     |
| Begin custom agent-first client at alpha                            | Accepted | Avoids client work before core server contract exists                 |
| Harden multi-user behavior during beta                              | Accepted | Functional support precedes production security/concurrency claims    |
| Begin custom browser Web Library after stable beta                  | Accepted | Browser complexity should not destabilize server foundations          |
| Keep agent provider credentials in native agent runtimes            | Accepted | Enables subscriptions without credential extraction                   |
| Separate tool adapters from optional session adapters               | Accepted | Claude Code and Pi expose different control surfaces                  |
| Use Pi as the first deep session adapter                            | Accepted | Pi provides SDK and RPC integration with native subscription auth     |
| Implement the server in Rust with Tokio and Axum                    | Accepted | Existing behavior is proven and the stack exposes exact HTTP semantics |
| Keep the public API on Zotero-compatible HTTP/JSON                  | Accepted | Official clients cannot consume a replacement gRPC boundary           |
| Use Rust types and traits for in-process application boundaries     | Accepted | Avoids premature serialization and distributed-system failure modes   |
| Reserve gRPC for a future independent native service if justified  | Accepted | Browser and agent boundaries remain easier to operate over JSON       |
| Keep WASM out of the pre-alpha server critical path                 | Accepted | It does not improve database, storage, or process durability           |
| Keep pure domain logic portable for optional client WASM later      | Accepted | Complex deterministic logic may eventually be shared with browsers    |
| Use nixpkgs RustFS for integration tests                            | Accepted | Removes a duplicate, unchecked package recipe from this repository    |
| Reproduce Zotero.org internal infrastructure                        | Rejected | Not required for public API compatibility                             |
