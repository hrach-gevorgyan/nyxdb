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
- [ ] On-disk size is within ~1.33x of real CouchDB per document (down
      from ~3.5x across two rounds of fixes — see `doc/BENCHMARKS.md`):
      zstd compression, `generate_id()` instead of a redundant counter,
      and a bincode-encoded revision tree instead of JSON-wrapped.
      **Tried and reverted**: a static "raw content" dictionary shared
      across documents (to reclaim cross-document redundancy, which
      sled's per-value compression can't see) — even after fixing an
      initial 10x throughput regression (caching the prepared dictionary
      instead of rebuilding it per write), disk size got *worse*, not
      better (2.31MB → 3.00MB), because per-frame zstd overhead
      (magic number, header, dictionary ID) outweighs redundancy savings
      for payloads this tiny (~200-400 bytes). Would need either a
      properly *trained* dictionary (requires sample data that doesn't
      exist ahead of time) or batching many documents into one
      compressed frame (bigger architecture change) to plausibly pay
      off — not attempted. Remaining untried candidate: packing rev ids
      as a raw `u64` generation + fixed-width hash bytes instead of a
      `"1-<hex>"` string — smaller, more contained change, modest
      expected payoff. Worth doing only if disk usage at real scale (not
      a 5,000-doc benchmark) turns out to matter — the gap is now small
      relative to the effort to close it further, and one plausible
      lever already failed empirically.
- [ ] `suggestions.md` (external optimization doc) proposed a faster
      zstd compression level to recover write throughput lost to
      compression. Tested directly (`COUCHDB_CLONE_COMPRESSION_LEVEL` env
      var, level 1 vs. default): no measurable effect on speed or size.
      For documents this small (~200-400 bytes, compressed
      independently), the effort-level knob doesn't have enough material
      to work with — the fixed per-call compression overhead dominates.
      Not a real lever for this workload; left in place as a knob in
      case it matters for larger documents in some other use case.

## Scope
- [ ] Do we ever need `_session` cookie auth (browser-direct, non-WebView
      client)? Not needed for current known use case.
- [ ] Attachments — is there a concrete use case yet, or still purely
      hypothetical?
- [ ] Discovery/pairing mechanism between client and server (mDNS? pairing
      code? manual URL entry?) — orthogonal to the protocol itself, but
      needs an answer before this is usable end-to-end.

## Process
- [x] Differential testing against real CouchDB — resolved: doesn't
      require Docker specifically, just *a* real CouchDB instance
      (Docker, a native install, or hosted). Verified against a native
      local CouchDB 3.5.2 install (`test/differential/run.js`). Manual/
      pre-release, not CI-blocking, since it needs that external instance.
