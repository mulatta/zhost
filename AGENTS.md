# Local agent instructions

## Documentation commit policy

- Documentation and planning files are local working material. Never stage or
  commit them.
- This includes, but is not limited to, `README.md`, `ROADMAP.md`,
  `TECHNICAL_DEBT.md`, `docs/**`, plans, architecture decision records, and
  other prose-only artifacts.
- Keep documentation changes in the working tree when implementing or
  committing code, configuration, migrations, or tests.
- Before creating any commit, inspect the staged diff and remove all
  documentation files from the commit scope without discarding their contents.
- Do not stage or commit this `AGENTS.md`.

## Code commit policy

- Commit code, configuration, migrations, and tests in logical, atomic units.
- Each commit must have one coherent purpose and include the tests or fixtures
  needed to verify that purpose.
- Do not mix unrelated cleanup, dependency upgrades, refactors, or behavior
  changes in one commit.
- Before committing, inspect both the working tree and staged diff, then stage
  only the files and hunks belonging to that unit.
- Run verification proportional to the change before creating the commit.
- Keep all documentation changes out of code commits.

## Remote repository policy

- Never run `git push` or otherwise update remote Git refs.
- Keep all commits local. The user alone decides if and when local commits are
  published.

## Agent coordination

- Project-local roles live in ignored `.codex/agents/*.toml`. Use the smallest
  role set needed for each atom:
  - `zhost-api-researcher`: read-only upstream contract evidence.
  - `zhost-contract-engineer`: exclusive test/harness writer.
  - `zhost-security-reviewer`: read-only independent gate.
  - `zotero-gui-operator`: serial temporary-app GUI E2E operator.
- The root agent owns production code, documentation synthesis, integration,
  staging, and commits.
- Give every subagent a base commit, one bounded slice, explicit read scope, an
  exclusive write allowlist, the expected red/green result, and a verification
  tier.
- Only one agent may write a path. Run research and review in parallel; never
  run a harness writer and GUI operator concurrently.
- Subagents never stage, commit, amend, rebase, reset, or push.
- Handoffs contain: scope, base, evidence, observed contract, zhost gap, files
  touched, verification command/task ID/exit status, risks, and next atom.

## Test harness

- The ignored local GUI harness is
  `.codex/harness/gui-e2e/zhost-gui`.
- Canonical automated integration remains the x86_64-linux NixOS test. The GUI
  harness is a manual compatibility gate, not a replacement.
- The GUI harness runs private PostgreSQL, RustFS, and zhost tasks in the
  dedicated `zhost-gui` pueue group on `psi`.
- Tunnel both `18189` (zhost) and `19000` (RustFS). Attachment downloads follow
  RustFS pre-signed URLs.
- Use only the temporary patched app, profile, and data directory recorded in
  the run manifest. Never launch or modify the installed Zotero app or normal
  profile/data.
- `get_app_state` launches its target when no matching process is running.
  After quitting temporary Zotero, verify shutdown with its recorded pueue task
  and `pgrep`; never call `get_app_state` as a shutdown check.
- Build the temporary Linux runtime from a `git+file://` flake so ignored Cargo
  targets and other scratch files never enter the Nix source closure.
- Preserve the one-time Zotero backup. Cleanup requires confirmed results and
  exact recorded pueue task IDs; never use group-wide kill or broad deletion.
