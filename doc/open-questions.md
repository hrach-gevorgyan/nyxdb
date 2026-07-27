# Open Questions

All resolved. Kept as a record of the decision and reasoning, since
"we decided not to do X" is worth remembering as much as "we did X."

## Deployment / security
- [x] **TLS or plaintext?** Resolved: plaintext HTTP, deliberately.
      This server is designed for a trusted LAN (a personal task-manager
      app syncing between a user's own devices), not internet-facing
      deployment. If a deployment ever needs to be internet-reachable,
      put TLS termination in front of it (Caddy/nginx/similar) rather
      than building TLS into the server itself — keeps this project's
      scope to the replication protocol, not transport security.
- [x] **CORS** — off by default, opt-in per-origin via
      `COUCHDB_CLONE_CORS_ORIGINS`, no wildcard. `db/src/main.rs::cors_layer`.
- [x] **Rate limiting** on `_bulk_docs` — resolved: not needed for the
      trusted-LAN deployment model above. Revisit if this is ever
      exposed beyond a trusted network.
- [x] **Credentials** — random per-install, or pinned via
      `COUCHDB_CLONE_USER`/`COUCHDB_CLONE_PASSWORD`. Required on every
      route except `GET /`. `db/src/auth.rs`.
- [x] **Request body size limit** — `DefaultBodyLimit`, 50MB default,
      override via `COUCHDB_CLONE_MAX_BODY_BYTES`.
- [x] **Cap on concurrent `feed=continuous` connections** — resolved:
      not needed for the trusted-LAN deployment model. Verified correct
      (not just fast) up to 80 concurrent in load testing.
- [x] **Docker image running as root** — fixed: `prod/Dockerfile` now
      creates and runs as a dedicated non-root user.
- [x] **`Credentials` deriving `Debug`** — fixed: manual `Debug` impl
      in `db/src/auth.rs` redacts the password.

## Architecture
- [x] `axum` vs `actix-web` — decided: axum. No reason found to revisit.
- [x] `sled` vs `rusqlite` — decided: sled, for embedded simplicity.
      Revisit only if sled's transactional guarantees become a real
      pain point in practice (none found so far).
- [x] Hash function for revision ids — decided: SHA-256 truncated.
      Byte-for-byte CouchDB compatibility was never a goal (see
      `USAGE.md` §7) — `new_edits:false` pushes carry the client's own
      hash regardless of algorithm, which is the only path real
      replication uses.
- [x] **Disk size is ~1.29x CouchDB's** (down from 3.5x — see
      `BENCHMARKS.md`). Tried dictionary compression twice, both made
      things worse (details in `changelog.md`). Decided: not worth
      pursuing further — the remaining gap is ~107 bytes/document, well
      under 2MB even at 20,000 documents. Revisit only if real usage
      ever shows otherwise.
- [x] Memory vs. real CouchDB — measured, this server is 2-3x lighter.
      Not a gap.
- [x] Panics on corrupted on-disk data — fixed: `storage.rs` returns
      `StorageResult`/`StorageError` instead of panicking.

## Scope
- [x] Attachments — done, Phase 4. See `USAGE.md`.
- [x] `_session` cookie auth, Mango/`_find` proxying — dropped from
      scope entirely. No concrete need arose during Phases 0–4.
- [x] Discovery/pairing between client and server — decided: out of
      this project's scope. The server only needs a URL and
      credentials; how a specific app obtains those (manual entry, QR
      code, mDNS, whatever) is an app-level concern independent of the
      protocol server. See `MIGRATING.md` for what an integrating app
      actually needs to provide.

## Process
- [x] Differential testing against real CouchDB doesn't need Docker —
      any real CouchDB instance works. `test/differential/run.js`.
