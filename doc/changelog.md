# Changelog

## [Unreleased]

**Phase 0 — spike**
- `GET /`, `PUT/GET /{db}`, `GET/PUT /{db}/{id}`, `_bulk_docs` implemented.
- Fixed: `PUT /{db}` on an existing db returned 200 instead of 412.
- Fixed: server crashed on `GET`/second `PUT` — `bincode` can't handle
  embedded `serde_json::Value` bodies. Switched to `serde_json` for storage.
- Verified against a real PouchDB client (`db.replicate.to()`). Fixed
  along the way: trailing-slash URLs didn't match routes (added
  `NormalizePathLayer`), error responses had empty bodies but PouchDB
  expects JSON (`ApiError` type), and `_local`/`_revs_diff` weren't
  implemented at all despite being required even for one-shot sync.

**Phase 1 — revision trees**
- Added revision-tree unit tests (conflicts, deletion, recreation, deep
  branches) — 14 tests total.
- `_bulk_docs` now supports `new_edits:false` — the real replication
  push format, creates genuine conflicts on divergence.
- `_bulk_get` added.
- `_conflicts` wired into `GET /{db}/{id}?conflicts=true`. Fixed: a
  deleted doc returned 200 with its tombstone instead of 404.

**Phase 2 — live replication**
- `_changes` in all three modes: normal, `feed=longpoll`, `feed=continuous`.
- Verified: two PouchDB instances running `db.sync({live:true})`
  converge in both directions, and resume correctly after a restart.

**Phase 3 — hardening**
- HTTP Basic auth added, required everywhere except `GET /`. Credentials
  are random per-install or pinned via env vars.
- CORS: off by default, opt-in per-origin, no wildcard.
- Load test added (`test/load/`). **Caught a real bug**: a
  `feed=continuous` subscriber that fell behind the broadcast channel
  silently dropped changes instead of catching up. Fixed by re-querying
  storage on lag instead of trusting the channel alone.
- Differential test against real CouchDB added (`test/differential/`).
  Matched exactly on the first run: winner-picking, conflicts,
  `_changes`, `_revs_diff`.

**Phase 4 — benchmarking and disk-size optimization**
- Added `doc/BENCHMARKS.md` — real comparison against local CouchDB.
  Found one real problem: this server used 3.5x more disk than CouchDB
  for the same 5,000 documents.
- Fixed it in two steps: enabled sled's zstd compression (off by
  default), and removed a redundant sled write per document (a
  hand-rolled sequence counter, replaced with sled's own `generate_id()`).
  Result: 6.1MB → 2.6MB (gap: 3.5x → 1.5x). Cost: write throughput
  dropped from ~56,800 to ~29,940 docs/sec — still ~8x faster than CouchDB.
- Found `DELETE /{db}` wasn't implemented at all (broke the differential
  test's own cleanup). Added it.
- Received an external optimization proposal (`suggestions.md`, since
  removed) and tested every claim instead of trusting it:
  - Two of its five "current" numbers were fabricated — idle memory
    claimed ~15MB (actually ~30.8MB), `_changes` latency claimed
    ~5-10ms (never measured before).
  - Real `_changes` latency: ~13.4ms end-to-end (HTTP overhead), but
    ~0.14ms for the actual in-process notification — already meets the
    "<1ms" target the proposal was trying to hit.
  - The proposed "<10MB active memory" target was below the real idle
    baseline — never achievable.
  - The proposed "faster zstd level" fix for write speed: tested
    directly, no measurable effect.
  - The one good idea — binary-encoding the revision tree instead of
    JSON-wrapping it — worked: 2.6MB → 2.31MB (gap: 1.5x → 1.33x), cost
    ~23,585 docs/sec (still ~7.2x faster than CouchDB).
- Removed `rust-couchdb-clone-plan.md` and `suggestions.md` — both
  fully consumed, content preserved in `USAGE.md`/`BENCHMARKS.md`/this
  file. Fixed all references. Recoverable from git history.
- **Dictionary compression, attempt 1: reverted.** Tried a shared
  compression dictionary to reclaim redundancy across documents that
  sled's per-value compression can't see. Made things worse: 2.31MB →
  3.00MB, and throughput dropped hard until the dictionary was cached
  properly. Root cause: fixed per-frame zstd overhead outweighs the
  benefit for documents this small. Reverted in full.
- Measured memory against real CouchDB for the first time. This server
  is ~3x lighter idle (~30.8MB vs ~93.5MB) and ~2x lighter under load
  (~56-60MB vs ~115.7MB). A real win, not a gap.
- Closed more of the disk gap safely: `bincode`'s default encoding uses
  fixed 8-byte length prefixes for every string, even tiny ones.
  Switched to varint encoding — smaller with no change to what's
  stored. Result: 2.31MB → 2.26MB (gap: 1.33x → 1.29x), and write speed
  improved slightly as a side effect (~26,178 docs/sec).
- **Dictionary compression, attempt 2: also reverted.** Retried with
  zstd's raw block API (the same technique sled uses internally, zero
  framing overhead) and a properly cached dictionary. Still made things
  worse (2.26MB → 3.0-3.1MB). Disabling sled's own compression to
  isolate the cause made it far worse (4.78MB) — sled's built-in
  compression already outperforms a generic untrained dictionary for
  this data. Two independent attempts failing the same way is enough
  evidence to stop. Reverted in full.

**Current state**: ~4.2MB install (~54x smaller than CouchDB), ~7.5x
faster writes, ~1.09x faster reads, 2-3x lighter memory, ~1.29x more
disk per document. Full numbers in `BENCHMARKS.md`.

**Ported real test cases from PouchDB's own test suite** instead of
relying only on our own hand-written cases:
- 5 winner-picking scenarios from `test.conflicts.js` ("Conflict
  resolution 1-5"), added as unit tests in `db/src/revtree.rs`. All
  passed immediately, including the one that specifically checks
  generation compares numerically, not as a string (`"10-a"` beats
  `"2-b"` — as plain strings `"10-a"` sorts first, which would be wrong).
- 4 HTTP-level cases in `test/ported/pouchdb_tests.js`: idempotent
  replay of the same `new_edits:false` push, deletion with full
  revision history, a doc id that collides with a JS prototype method
  name (`"constructor"`), and an empty `_revs_diff` request. All pass.
- Explicitly did not port PouchDB tests for features this server
  doesn't implement (optimistic concurrency on plain `PUT`, `revs_limit`,
  `open_revs=all`) — those are documented, deliberate gaps
  (`USAGE.md` §7), not bugs to fix by copying a test.
- Rewrote all documentation to be plainer and more direct — no content
  lost, just less narrative padding.
- **Full project audit** before starting Phase 4 (see `AUDIT.md` for
  the process and full findings). Dependency scan (`cargo audit`, 205
  crates): no known vulnerabilities, 3 unmaintained-only warnings.
  Live-tested error paths and found the same bug class already fixed
  once in Phase 0 (non-JSON error bodies) in three more places:
  malformed JSON body, wrong/missing `Content-Type`, and genuinely
  unmatched routes all returned plain-text or empty responses instead
  of this project's JSON error format. Fixed with a single outermost
  middleware (`normalize_error_body` in `db/src/routes.rs`) that
  rewrites any non-JSON 4xx/5xx response.
  Also fixed: HTTP Basic auth compared credentials with plain `==` (a
  timing side-channel — closed with a constant-time comparison), and
  `credentials.json` had no explicit file permissions (world-readable
  by default on the Linux/Docker deployment path — fixed to `0o600` on
  Unix, unverified on this Windows dev machine since the `#[cfg(unix)]`
  code doesn't compile in here).
  Deferred two real findings that need more care than a quick fix: no
  request body/batch size limit (memory-exhaustion vector, but the
  right limit is a judgment call), and `storage.rs` panicking on
  corrupted on-disk data instead of failing gracefully (touches the
  core read path, deserves its own focused pass). Both logged in
  `open-questions.md`, along with lower-priority review items (rate
  limiting, connection caps, Docker running as root, `Credentials`
  deriving `Debug`).
  Verified no regression: 19 unit tests, both PouchDB integration
  tests, the load test, the differential test against real CouchDB,
  and all ported PouchDB test cases still pass.
