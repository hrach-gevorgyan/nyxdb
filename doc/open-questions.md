# Open Questions

Decisions deliberately deferred. Resolve explicitly, don't inherit a
default silently.

## Deployment / security
- [ ] **TLS or plaintext?** Depends on deployment (LAN-only vs.
      internet-reachable). Not decided.
- [x] **CORS** — off by default, opt-in per-origin via
      `COUCHDB_CLONE_CORS_ORIGINS`, no wildcard. `db/src/main.rs::cors_layer`.
- [ ] **Rate limiting** on `_bulk_docs` — needed or not?
- [x] **Credentials** — random per-install, or pinned via
      `COUCHDB_CLONE_USER`/`COUCHDB_CLONE_PASSWORD`. Required on every
      route except `GET /`. `db/src/auth.rs`.
- [x] **Request body size limit** — resolved: `DefaultBodyLimit`, 50MB
      default, override via `COUCHDB_CLONE_MAX_BODY_BYTES`.
- [ ] **No cap on concurrent `feed=continuous` connections** (from
      `AUDIT.md`). Verified correct up to 80 concurrent; not verified
      against resource exhaustion at much larger scale.
- [ ] **Docker image runs as root** (from `AUDIT.md`) — no `USER`
      directive. Standard hardening, low urgency for a LAN deployment.

## Architecture
- [ ] `axum` vs `actix-web` — using axum, not revisited.
- [ ] `sled` vs `rusqlite` — using sled, revisit if transactions become
      a pain point.
- [ ] Hash function for revision ids — SHA-256 truncated, fine unless
      byte-for-byte CouchDB interop is ever needed.
- [ ] **Disk size is ~1.29x CouchDB's** (down from 3.5x — see
      `BENCHMARKS.md`). Tried dictionary compression twice, both
      reverted — made things worse, not better (details in
      `changelog.md`). Two untried options if this ever needs to close
      further: a properly trained dictionary (needs real sample data we
      don't have), or packing revision ids as raw bytes instead of hex
      strings (touches conflict-resolution comparison logic — real
      risk for a small gain). Not worth it at current scale.
- [x] Memory vs. real CouchDB — measured, this server is 2-3x lighter.
      Not a gap.
- [x] **Panics on corrupted on-disk data** — resolved: `storage.rs` now
      returns `StorageResult`/`StorageError` instead of panicking on
      bad deserialization.
- [ ] **`Credentials` derives `Debug`** (from `AUDIT.md`). Not logged
      anywhere today, but a future `{:?}` on `AppState` would leak the
      password. Cheap to harden (manual redacted `Debug` impl) even
      though nothing exploits it currently.

## Scope
- [ ] Attachments — the only remaining Phase 4 item.
- [ ] Discovery/pairing between client and server (mDNS? pairing code?)
      — separate from the protocol itself, needs an answer eventually.

`_session` cookie auth and Mango/`_find` proxying are dropped from
scope entirely — not "no concrete need yet," just not planned.

## Process
- [x] Differential testing against real CouchDB doesn't need Docker —
      any real CouchDB instance works. `test/differential/run.js`.
