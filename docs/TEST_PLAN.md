# zhost test plan

This file is local planning material and must not be committed.

## Purpose

Test Profile A against both its production-shaped Linux environment and the
official Zotero desktop client. NixOS tests are the repeatable contract gate.
GUI runs are a compatibility oracle and are trimmed only after their request
contract is captured in automated tests.

## Verification tiers

### Tier 0: fast code checks

Run before every code commit:

- `cargo test`
- `cargo clippy --all-targets -- -D warnings`
- `cargo fmt --check`
- treefmt/flake formatting check
- shellcheck for changed harness scripts

### Tier 1: x86_64-linux NixOS integration

Offload from macOS to the configured Linux builder. Use real PostgreSQL and
RustFS, not mocks.

- `checks.x86_64-linux.nixos-sync`
  - metadata/settings/collections/searches/full-text sync
  - conditional writes, conflicts, deletions, pagination, and query formats
  - attachment authorization, upload, registration, download, and cleanup
  - compressed Zotero ZIP contract with separate original/blob hashes
  - wrong ZIP hash rejection
  - pending upload survival across authorization/upload restarts
  - S3 `application/zip` metadata and compressed response headers
- `checks.x86_64-linux.nixos-key-auth`
  - populated-schema migration through latest version
  - OIDC identity mapping and ownership
  - DB-backed scoped keys and revocation
  - personal/group authorization and role boundaries
  - restart and startup-failure behavior

### Tier 2: official Zotero GUI compatibility

Run serially with ignored `.codex/harness/gui-e2e/zhost-gui`.

- remote x86_64-linux PostgreSQL, RustFS, and zhost remain in dedicated pueue
  tasks;
- local tunnel exposes zhost and RustFS only on loopback;
- Alice and Bob use separate temporary profiles and data directories;
- temporary patched Zotero app points at zhost;
- one-time normal Zotero backup remains preserved;
- after each novel client failure, add Tier 1 regression first, fix, then retry
  GUI.

Profile A smoke sequence:

1. Alice login and private-group discovery.
2. Alice creates group metadata and text attachment.
3. Alice uploads attachment through stock compressed ZIP flow.
4. Bob login and group discovery.
5. Bob downloads attachment and extracted bytes match Alice source.
6. Bob edits group metadata and syncs.
7. Alice syncs and observes Bob title and attribution.
8. Audit DB membership, active keys, item/file metadata, and migration version.
9. Collect logs, confirm results, leave remote services running when requested.

## Completed compatibility run

Run `20260723T190144Z-setup`, 2026-07-24:

- migrations 1 through 13 applied;
- Alice owner and Bob member discovered same private group;
- attachment `AMUZNHPL` registered with original MD5
  `f52d8b29b7483548b49b50dcc9c7043f`, ZIP MD5
  `cc579afbef61779e103e529a3533cd84`, ZIP size 215, compressed flag true;
- Bob downloaded 59-byte extracted file with SHA-256
  `925cce519387ff138fcf631da9e113318346a7af075e2685f87699794a98d5a9`,
  matching Alice source;
- Bob changed item `4MUSYZN4` to
  `Bob Reviewed Alice Group Attachment E2E`;
- Alice observed changed title and `Modified By Bob`;
- both temporary Zotero tasks exited successfully;
- remote PostgreSQL, RustFS, zhost, and local tunnel intentionally remain
  running;
- artifacts live under
  `/Users/seungwon/.claude/outputs/zhost-gui-e2e/runs/20260723T190144Z-setup/artifacts`.

## Safety note from completed run

One shutdown check called `get_app_state` after Zotero had exited. Computer Use
therefore launched the temporary app without profile arguments and opened the
normal profile briefly. No edit was made. Credential files remained byte-for-
byte identical to backup; all user-library tables remained logically identical.
Only `translatorCache` rows and repository/last-check timestamps changed.
Never use `get_app_state` to verify shutdown.

## Current status

As of 2026-08-26, branch `zhost-multi-user-support` is 42 commits ahead of
`main`; latest code commit is `c01de2d server: add root Cargo workspace`.
Production code is clean. Only local documentation and planning files are dirty.

Recent verification:

- `cargo fmt`
- `cargo clippy --all-targets --all-features`
- `cargo test --manifest-path server/Cargo.toml` — 12 passed
- `nix build .#packages.x86_64-linux.zhost`
- `nix build .#checks.x86_64-linux.nixos-sync .#checks.x86_64-linux.nixos-key-auth`

Structure status:

- repository root now has a Cargo workspace, so `cargo clippy` works from the
  repository root;
- Nix expressions live under `nix/`;
- `server/src/main.rs` is reduced to bootstrap logic;
- HTTP handlers live under `server/src/handlers/`;
- request access, middleware, headers, and validation live under
  `server/src/http/`;
- store facade remains in `server/src/store/mod.rs`, with identity, versions,
  objects, files, full text, groups, sessions, settings, and tags split into
  dedicated modules.

## GUI E2E execution plan

Use only the ignored temporary GUI harness at
`.codex/harness/gui-e2e/zhost-gui`. Never launch the installed Zotero app or the
normal profile. Confirm temporary patched app, profile, data directory, zhost
URL `http://127.0.0.1:18189`, and loopback RustFS tunnel before each major
section.

Execution order:

1. Conflict behavior.
2. Metadata round trips.
3. Attachment edge cases.
4. Full-text edge cases.
5. Network interruption and retry.
6. Restart and persistence.
7. Deletion and restore.
8. Final security and normal-profile integrity audit.

Conflict work starts from the current checkpoint: server title
`Alice Conflict Winner E2E v2` at version 8, then stale Bob attempts
`Bob Conflict Candidate E2E v2` and sync behavior is recorded. Required
observations include whether a conflict dialog appears, whether local/remote
values are correct, remote and local resolution outcomes, cancel behavior,
absence of silent stale overwrite, field-level merge behavior, attachment
metadata conflicts, and delete-vs-update conflicts.

## Remaining release-gate work

- Finish the GUI checklist in the order above, then trim each novel observed
  client contract into Tier 1 NixOS regressions.
- Exercise owner/admin/member transitions and immediate removal/revocation in
  one desktop session where client behavior adds value beyond Tier 1.
- Add backup/restore drill covering metadata, full text, original item hashes,
  compressed blob hashes, and S3 bytes.
- Add bounded-memory streaming upload coverage before production-size files.
- Resolve or expire three legacy pending rows from the failed pre-migration ZIP
  attempts; they cannot register because migration cannot reconstruct ZIP hash.
