# Open Questions

Decisions deliberately deferred — don't inherit defaults silently, resolve
each explicitly before the phase that needs it (see plan §7).

## Deployment / security
- [ ] TLS or plaintext HTTP? Depends on deployment model (LAN-only vs.
      internet-reachable). Not yet decided.
- [x] CORS — resolved: disabled by default (same-origin only, doesn't
      affect non-browser clients). Set `COUCHDB_CLONE_CORS_ORIGINS` to a
      comma-separated allowlist to enable it for a specific browser/WebView
      origin; no wildcard support, so a real origin must be named
      explicitly. See `db/src/main.rs::cors_layer`.
- [ ] Rate limiting on `_bulk_docs` / abuse protection — needed or not?
- [x] Credential storage/generation mechanism — resolved: random
      per-install (`admin` + generated password) written to
      `<data dir>/credentials.json`, or pinned via
      `COUCHDB_CLONE_USER`/`COUCHDB_CLONE_PASSWORD`. HTTP Basic auth
      required on every route except `GET /`. See `db/src/auth.rs`.

## Architecture
- [ ] `axum` vs `actix-web` — leaning `axum`, not finalized.
- [ ] `sled` vs `rusqlite` for storage — leaning `sled` for simplicity,
      revisit if transactional guarantees become a pain point.
- [ ] Hash function for revision ids — SHA-256 truncated is the current
      lean; only matters if byte-for-byte CouchDB interop is ever needed.

## Scope
- [ ] Do we ever need `_session` cookie auth (browser-direct, non-WebView
      client)? Not needed for current known use case.
- [ ] Attachments — is there a concrete use case yet, or still purely
      hypothetical?
- [ ] Discovery/pairing mechanism between client and server (mDNS? pairing
      code? manual URL entry?) — orthogonal to the protocol itself, but
      needs an answer before this is usable end-to-end.

## Process
- [ ] Differential testing against real CouchDB requires Docker — confirm
      this is acceptable as a manual/pre-release gate rather than CI-blocking.
