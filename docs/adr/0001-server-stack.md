# ADR 0001: Server stack and protocol boundaries

- Status: Accepted
- Date: 2026-07-24

## Context

zhost must reproduce the public Zotero HTTP API closely enough for official
clients while growing from a single-user sync server into a multi-user server
with groups and roles. The implementation also needs PostgreSQL transactions,
S3-compatible attachment storage, browser login through an OIDC reverse proxy,
and later agent and custom-client integrations.

Framework, RPC, and portable-runtime choices must not obscure Zotero's unusual
HTTP headers, conditional writes, content types, redirects, query parameters, or
file-transfer behavior.

## Decision

The server remains a native Rust service built on:

- Tokio for the asynchronous runtime;
- Axum 0.8 for HTTP routing and extraction;
- Tower and Tower HTTP for middleware;
- SQLx and PostgreSQL for durable state and transactions;
- an S3-compatible client behind a project-owned storage interface.

The public boundary remains Zotero-compatible HTTP/JSON. Custom browser and
agent tool APIs also start as HTTP/JSON. Agent event streams may use WebSocket.
gRPC is not a canonical application boundary and may be introduced only for a
future independently deployed native service with a demonstrated streaming or
code-generation need.

In-process boundaries use Rust types, functions, and traits. They do not use a
network serialization protocol.

WASM is not part of the pre-alpha server. Pure deterministic domain logic should
avoid Axum, SQLx, filesystem, and process dependencies so a browser adapter can
compile selected logic with `wasm-bindgen` later. A server-side WASM plugin host
requires a separate post-1.0 design and capability model.

Loco, Actix Web, and Rocket are not adopted:

- Actix Web is viable but offers no protocol or durability improvement that
  justifies rewriting the existing Axum server.
- Rocket provides less direct control over this compatibility-heavy API and has
  a slower release cadence.
- Loco is an opinionated Axum distribution centered on SeaORM, JWT accounts,
  mail, and background jobs. Those conventions conflict with direct SQLx
  transactions, reverse-proxy OIDC identity, and Zotero API keys.

RustFS used by integration tests comes from the pinned nixpkgs input. zhost does
not maintain a duplicate RustFS package recipe.

## Consequences

- Existing verified Zotero behavior remains reusable.
- HTTP compatibility tests can invoke the Axum router as a Tower service.
- Application state and authorization must be extracted from current globals
  into explicit Rust types.
- Durability comes from PostgreSQL, object storage, idempotency, and migrations,
  not Tokio tasks, gRPC, or WASM.
- Browser clients do not depend on gRPC-Web or a WASM UI framework.
- Dependency upgrades remain separate changes from authorization and schema
  refactors.

## Revisit conditions

Revisit only when evidence shows one of these conditions:

- Axum blocks a required public Zotero API behavior.
- A separately deployed native service needs bidirectional streaming and
  generated clients that HTTP/JSON or WebSocket cannot provide adequately.
- Shared client logic is large and complex enough that a WASM boundary reduces
  verified duplication.
- Third-party server extensions require sandboxed execution and a versioned WIT
  capability interface.
