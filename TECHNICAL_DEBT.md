# Technical debt

This ledger tracks implementation liabilities in the current pre-alpha server.
Missing endpoints in the planned public Zotero API are product work and belong
in the API compatibility inventory; they are listed here only when the current
implementation makes that work unsafe or unnecessarily difficult.

Priorities:

- **P0** blocks safe multi-user work or can violate current data/security
  invariants.
- **P1** blocks an alpha-quality operational or compatibility claim.
- **P2** should be addressed before a stable release but does not block the next
  vertical slice.

## Current status

- [x] Move Nix packages, modules, and NixOS checks under `nix/`.
- [x] Add a root Cargo workspace so repository-root `cargo clippy` works.
- [x] Split Rust bootstrap, handlers, HTTP helpers, and store modules into
  focused files while keeping the store facade API stable.
- [x] Validate the current code with `cargo fmt`, `cargo clippy`, `cargo test`,
  `nix build .#packages.x86_64-linux.zhost`, and the `nixos-sync` /
  `nixos-key-auth` checks.

## P0: immediate cleanup

- [x] Replace `pkgs/rustfs/default.nix` with nixpkgs' `rustfs` package and
  `services.rustfs` NixOS module. The pinned nixpkgs now supplies
  `1.0.0-beta.9`; the old local `1.0.0-alpha.72` recipe skipped all tests.
- [x] Record server framework, HTTP, gRPC, WASM, and RustFS decisions in an ADR.
- [x] Make README status text agree with the API-first multi-user roadmap.
- [ ] Create the machine-readable public API inventory and assign each route to
  a compatibility profile.

## P0: library and authorization boundaries

- [ ] Remove `const LIBRARY_ID: i64 = 1` from `server/src/store.rs` and pass a
  typed `LibraryId` to every store operation.
- [ ] Resolve `/users/{id}` and `/groups/{id}` to an authorized library instead
  of ignoring the path identifier.
- [ ] Introduce explicit `UserId`, `GroupId`, `LibraryId`, `Principal`,
  `Permissions`, and `RequestContext` types.
- [ ] Replace process globals (`CFG`, `POOL`, and `STORAGE`) with Axum
  `State<AppState>` so tests and services receive explicit dependencies.
- [ ] Replace boot-time plaintext API-key maps and the shared login key with
  DB-backed, user-owned, hashed, scoped, revocable keys.
- [ ] Map trusted OIDC `(issuer, subject)` claims to local users. Email remains
  display/contact data rather than an identity key.
- [ ] Persist login sessions and pending uploads in PostgreSQL. Current in-memory
  maps lose state on restart and prevent multiple server instances.

## P0: attachment integrity and resource use

- [ ] Namespace object-storage keys by library. Current bare item keys collide
  when two libraries use the same Zotero key.
- [ ] Make attachment replacement crash-consistent. Current upload handling
  overwrites the final S3 object before file metadata is registered, so a crash
  can leave old DB metadata pointing at new bytes.
- [ ] Decide and test an immutable/versioned blob-key scheme, atomic DB pointer
  update, and orphan cleanup policy.
- [ ] Stream attachment bodies through bounded hashing and S3 upload. Current
  middleware buffers every request up to 256 MiB, and the S3 client requires a
  complete byte slice.
- [ ] Remove request-body content from default info logs. Metadata, notes, full
  text, and binary prefixes are user data.
- [ ] Move gzip decompression off synchronous reads on Tokio workers or adopt a
  bounded asynchronous request-decompression path.
- [ ] Stop converting database failures into library version `0`; return an
  explicit server error instead.

## P1: schema and migrations

- [ ] Add relational users, external identities, API keys, libraries, personal
  libraries, groups, memberships, login sessions, and pending uploads.
- [ ] Add constraints for ownership, membership, object kinds, and key formats
  where they enforce durable invariants without rejecting unknown upstream JSON
  fields.
- [ ] Test migrations from realistic previous schemas and populated databases.
- [ ] Document backup-before-upgrade and rollback-by-restore procedures.
- [ ] Add an explicit `zhost migrate` operational path before multi-instance
  deployment; startup migrations alone are insufficient for controlled rollout.
- [ ] Audit `IF NOT EXISTS` migration statements that can conceal schema drift.
- [ ] Update stale migration comments, including the old filesystem description
  for S3-backed file bytes.

## P1: HTTP and compatibility structure

- [ ] Split the 1,265-line `main.rs` into configuration, application, auth,
  route, error, and storage modules without changing external behavior.
- [ ] Centralize Zotero error, version-header, pagination, and conditional-write
  responses so handlers cannot silently omit contract details.
- [ ] Separate current verified behavior in `server/SPEC.md` from target support
  in `docs/api/`.
- [ ] Inventory API-version, schema-version, content-negotiation, conditional
  request, pagination, and output-format edge cases.
- [ ] Add a trusted-proxy configuration and fail closed when identity headers can
  arrive from an untrusted network path.
- [ ] Remove the known `zhost-dev-key` fallback unless an explicit development
  mode is enabled.

## P1: operations

- [ ] Handle `SIGTERM` as a graceful shutdown signal and verify in the NixOS
  service test.
- [ ] Add liveness and readiness endpoints with documented PostgreSQL and object
  storage semantics.
- [ ] Configure database pool, request, S3, and shutdown timeouts.
- [ ] Add request concurrency and upload-specific resource limits.
- [ ] Add structured audit events for login approval, key lifecycle, membership,
  role, and administrative changes without logging secrets or library content.
- [ ] Exercise backup and restore for metadata, full text, and attachment bytes.

## P1: test and quality gates

Current coverage contains eight Rust query unit tests and forty NixOS HTTP
integration subtests. Preserve these as the single-user regression baseline.

- [ ] Finish official Zotero desktop GUI E2E checklist; direct HTTP integration
  tests are not a client-compatibility substitute.
- [ ] Add two-user, two-library, same-object-key isolation tests.
- [ ] Add owner/admin/member and key-permission matrix tests.
- [ ] Add concurrent write, process-restart, and attachment-registration failure
  tests.
- [ ] Add migration, backup, and restore tests.
- [ ] Add large streaming-upload and bounded-memory tests.
- [ ] Run formatting, `cargo test`, Clippy, individual NixOS checks, and
  dependency advisory/license checks in CI.

## P2: dependencies and abstractions

- [ ] Audit the S3 client before streaming work. Current `rust-s3 0.35.1` pulls
  Hyper 0.14/http 0.2 alongside Axum's Hyper 1/http 1 stack. Compare current
  `rust-s3`, `aws-sdk-s3`, OpenDAL, and `object_store` against R2 and RustFS with
  presigning and streaming tests.
- [ ] Evaluate SQLx 0.9 in an isolated dependency change. Do not combine it with
  identity schema or authorization refactors.
- [ ] Introduce a narrow object-storage trait when attachment tests can define
  its required semantics; do not create a generic storage framework first.
- [ ] Keep pure deterministic domain logic portable, but add `wasm-bindgen` only
  after a custom client demonstrates enough shared logic to justify the boundary.
- [ ] Consider gRPC only for a future independently deployed native service with
  a concrete bidirectional-streaming or generated-client requirement.

## Resolved decisions

- Server remains Rust, Tokio, and Axum.
- Public Zotero compatibility remains HTTP/JSON.
- In-process boundaries use Rust types and traits rather than RPC.
- One deployment is one tenant containing multiple users and groups.
- PostgreSQL and object storage provide durability; runtime tasks do not.
- WASM is not part of the pre-alpha server path.
- Loco, Actix Web, and Rocket are not adopted for the server.
