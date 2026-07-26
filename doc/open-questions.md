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
- [ ] On-disk size is within ~1.29x of real CouchDB per document (down
      from ~3.5x across three rounds of fixes — see `doc/BENCHMARKS.md`):
      zstd compression, `generate_id()` instead of a redundant counter,
      a bincode-encoded revision tree instead of JSON-wrapped, and
      varint integer/length encoding (`bincode_options()` in
      `storage.rs`) instead of bincode's default fixed-width 8-byte
      prefixes. **Tried and reverted twice**: a static "raw content"
      dictionary shared across documents (to reclaim cross-document
      redundancy, which sled's per-value compression can't see) — first
      via zstd's streaming frame API (per-frame overhead outweighed the
      benefit for ~200-400 byte payloads: 2.31MB → 3.00MB), then via
      zstd's raw block API with a cached prepared dictionary (the same
      zero-overhead primitive sled itself uses internally — technically
      sound, still made things worse: 2.26MB → 3.01-3.07MB across two
      compression levels tested). Disabling sled's own compression
      entirely to isolate the cause made it far worse still (4.78MB),
      showing sled's own plain per-item compression already outperforms
      a generic untrained ~2KB dictionary for this data. Two
      independent implementations failing the same way is strong enough
      evidence to stop pursuing generic-dictionary compression here.
      Remaining candidates, neither attempted: (1) a *properly trained*
      dictionary (`ZDICT_trainFromBuffer` against real sample data,
      which doesn't exist ahead of time for an arbitrary app's
      documents — would need to be trained adaptively after some number
      of real writes, a real architecture change); (2) packing rev ids
      as raw `u64` generation + fixed-width hash bytes instead of a
      `"1-<hex>"` string — the hash string is compared byte-for-byte in
      winner-picking's tiebreak, so repacking it risks a subtle
      round-trip mismatch in exactly the code path differential testing
      exists to protect, for a modest expected payoff now that varint
      encoding already captured most of the easy win. Worth revisiting
      either only if disk usage at real scale (not a 5,000-doc
      benchmark) turns out to matter.
- [ ] `suggestions.md` (external optimization doc) proposed a faster
      zstd compression level to recover write throughput lost to
      compression. Tested directly (`COUCHDB_CLONE_COMPRESSION_LEVEL` env
      var, level 1 vs. default): no measurable effect on speed or size.
      For documents this small (~200-400 bytes, compressed
      independently), the effort-level knob doesn't have enough material
      to work with — the fixed per-call compression overhead dominates.
      Not a real lever for this workload; left in place as a knob in
      case it matters for larger documents in some other use case.
- [x] Memory usage vs. real CouchDB — resolved: measured on both sides
      (previously only ever measured for this server). This server is
      ~3x lighter idle (~30.8MB vs. ~93.5MB) and ~2x lighter under a
      comparable load (~56-60MB vs. ~115.7MB, both working set via
      `Get-Process`). Not a gap to close — a real, verified advantage.

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
