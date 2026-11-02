# ADR 0002: Web Library as the only client

- Status: Accepted
- Date: 2026-11-02

## Context

xotero started as a sync target for a URL-patched Zotero desktop app. ADR 0001
also left room for later custom clients, and the earlier roadmap planned a
dedicated agent-first client and a custom browser client.

Supporting several client surfaces multiplies packaging, compatibility testing,
and credential boundaries. Upstream `zotero/web-library` already covers the library
workflows that this server needs.

## Decision

- Pinned upstream `zotero/web-library` is the only supported client.
- Desktop Zotero support ends. The patched `zotero` package and
  `homeModules.zotero` are removed from the supported outputs.
- There is no dedicated client and no custom browser client.
- Browser access uses OIDC-backed, server-side web sessions.
- `/keys/sessions` and `/login` stay available as a browser-approved way to issue
  user-owned API keys.
- Agents use the public API through tool adapters. They do not get a separate
  client surface.

This supersedes the custom-client direction in ADR 0001. The rest of ADR 0001
stands.

## Consequences

- Compatibility work follows requests from pinned Web Library, and a real browser
  check replaces the desktop GUI gate.
- The login session path remains a credential-issuing surface, so it stays under
  the same authorization and test coverage as other key paths. It keeps
  `loginKdfKeyFile` and the `login_sessions` table.
- Keeping the login path does not make desktop Zotero a supported client.
- Removing the login path later needs a new decision, a forward migration, and
  test migration.
