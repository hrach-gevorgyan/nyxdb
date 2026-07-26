# Changelog

All notable changes to this project are documented here.
Format loosely follows [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]
- Project scaffolding created (doc/, db/, test/, prod/).
- Phase 0 spike: `GET /`, `PUT/GET /{db}`, `GET/PUT /{db}/{id}` implemented
  and manually verified against a running server.
- Fixed: `PUT /{db}` on an existing database returned 200 instead of 412
  (existence check compared against the wrong sled tree name).
- Fixed: server panicked on every `GET`/second `PUT` because `RevTree` was
  serialized with `bincode`, which can't deserialize the embedded
  `serde_json::Value` doc bodies. Switched storage encoding to `serde_json`.
- Added `POST /{db}/_bulk_docs` (batch write, last-write-wins per doc,
  matches `put_doc`'s generation logic). Does not yet honor
  `new_edits:false` — deferred to Phase 1 where real revision trees land.
- Verified Phase 0 against a real PouchDB client (`test/integration/run.js`,
  `db.replicate.to()`). Fixed along the way:
  - Trailing-slash URLs (`GET /{db}/`) didn't match our routes — PouchDB's
    http adapter always requests db/doc info with a trailing slash. Added
    `tower_http::normalize_path::NormalizePathLayer`.
  - Error responses had empty bodies; PouchDB expects JSON even on
    404/400. Added an `ApiError` type returning
    `{"error":..,"reason":..}` consistently.
  - `_local` checkpoint docs (`GET/PUT /{db}/_local/{id}`) and
    `POST /{db}/_revs_diff` weren't implemented at all — both are
    required even for a trivial one-shot push, not just live sync.
    Added minimal versions (single-revision last-write-wins for
    `_local`; simple presence check, no `possible_ancestors`, for
    `_revs_diff`).
- Phase 1 start: added revision-tree unit tests for unequal-depth
  conflicts, resolved conflicts (deleted branch preserved not pruned),
  fully-deleted docs, deleted-then-recreated docs, deep multi-generation
  branches, and `_revs_diff`-style missing-revs lookups. All pass against
  the existing winner-picking implementation. Refactored `_revs_diff` to
  use the new `RevTree::missing` helper instead of duplicating the logic.
- Added `RevTree::insert_revision_chain` plus 6 unit tests (idempotency,
  connecting to existing history, never clobbering an existing tombstone,
  and — the actually load-bearing case — a diverging chain correctly
  creating a real conflict instead of overwriting).
- Wired `new_edits:false` into `POST /{db}/_bulk_docs`: the pushing side
  of real replication now has its exact `_rev`/`_revisions` history
  stored verbatim instead of the server always minting a fresh
  last-write-wins rev. Verified manually over HTTP: a full 3-generation
  push, followed by a diverging push at the same parent, correctly
  produces a real conflict resolved by the existing hash tiebreak.
- Wired `_conflicts` into `GET /{db}/{id}?conflicts=true` (only present
  in the response when non-empty, matching real CouchDB). Also fixed: a
  fully-deleted doc previously returned 200 with its tombstone body;
  `ApiError` now carries a distinct `reason` per case, and `get_doc`
  returns 404 `{"error":"not_found","reason":"deleted"}` for it. Verified
  manually over HTTP (conflict listing, and delete-then-404 sequence) and
  confirmed no regression in unit tests or the PouchDB integration test.
- Added `POST /{db}/_bulk_get` (plan §3): batched doc fetch, one ok/error
  entry per requested `{id, rev?}` pair rather than failing the whole
  batch on a miss. Verified over HTTP: existing doc without `rev`, a
  nonexistent id, and a bogus `rev` on a real id all report correctly.
  This closes out Phase 1 — full unit suite (14 tests) and the PouchDB
  integration test both still pass.
- Phase 2 complete: `GET /{db}/_changes` in all three delivery modes
  (normal, `feed=longpoll`, `feed=continuous`). Added a per-database
  `ChangeFeed` broadcast channel (`ChangeFeedRegistry` in
  `db/src/changes.rs`), wired into every write path (`put_doc`, both
  `_bulk_docs` modes). Continuous mode streams newline-delimited JSON:
  catch-up rows from storage first, then live rows via the broadcast
  channel. Longpoll re-derives from storage after waking rather than
  trusting the single event that woke it, so a burst of coalesced writes
  isn't half-missed.
  Verified with a new acceptance test (`test/integration/live_sync.js`):
  two independent PouchDB instances both running
  `db.sync({live:true, retry:true})` against the server converge in both
  directions without polling or restarting — the actual real-world usage
  shape from plan §2.2. Also manually verified that a doc, its `_local`
  checkpoint, and `_changes` sequence numbers all survive a server
  restart, confirming resumable sync actually works end-to-end.
- Added HTTP Basic auth (plan §5, §7), required on every route except
  `GET /` (server identification stays public, matching real CouchDB's
  behavior — clients probe it for feature detection before necessarily
  having db-specific credentials). Credentials are random per-install
  (`admin` + generated password, written to `<data dir>/credentials.json`
  on first run) or pinned via `COUCHDB_CLONE_USER`/
  `COUCHDB_CLONE_PASSWORD`, which always take priority. New `db/src/auth.rs`.
  Verified over HTTP: public root, 401 with no credentials, 200 with
  correct credentials, 401 with wrong credentials. Updated both
  integration tests (`run.js`, `live_sync.js`) to authenticate; both
  still pass, as do all 14 unit tests.
- Resolved the CORS open question (plan §7): disabled by default (no
  layer at all, same-origin only), opt-in per-origin via
  `COUCHDB_CLONE_CORS_ORIGINS` — no wildcard support, so a real origin
  has to be named. Applied as the outermost layer, ahead of auth, since
  a CORS preflight never carries credentials and would otherwise 401
  before the CORS layer got a chance to answer it. Verified with curl:
  allowed origin gets `access-control-allow-origin`, a disallowed one
  doesn't, and preflight correctly bypasses auth either way.
- Added a load/soak test (`test/load/run.js`, plan §6.5): a large
  `_bulk_docs` batch plus many concurrent `feed=continuous` subscribers,
  verifying every subscriber actually receives every change.
  **This caught a real bug**: the continuous handler's `filter_map`
  treated `RecvError::Lagged` (a subscriber falling behind the broadcast
  channel's fixed capacity) the same as a clean `Ok` via `.ok()?` —
  silently discarding whatever was missed instead of catching up. Passed
  fine at 20 subscribers / 2000 docs, failed at 50 subscribers / 8000
  docs (one subscriber stalled at 3665/8000, never recovering).
  Fixed by rewriting `changes_continuous` as a stateful
  `futures::stream::unfold` (`ContinuousState`) that, on `Lagged`,
  re-queries `db.changes_since(last_seq)` and resumes from there instead
  of just resubscribing and hoping. Verified at 50 subscribers/8000 docs
  and 80 subscribers/15,000 docs with zero missed changes at either
  scale. Dropped the now-unused `tokio-stream` dependency.
- Added differential testing against real CouchDB (`test/differential/run.js`,
  plan §6.2) — the last Phase 3 item. Drives both servers with identical
  `new_edits:false` pushes using explicit shared rev ids on both sides,
  so the comparison isolates conflict/winner-picking logic rather than
  the two servers' different hash algorithms. Builds a tree with a
  conflict, a deeper-generation resolution, a deletion, and a
  recreation, then diffs winning rev, `_conflicts`, `_changes` content,
  and `_revs_diff`. Verified against a real local CouchDB 3.5.2 —
  everything matched exactly on the first run. This closes out Phase 3.
- Added `doc/BENCHMARKS.md` and `test/benchmark/vs_couchdb.js`: a direct,
  reproducible comparison against real local CouchDB 3.5.2 using
  disposable databases on both. Real numbers from one run each: install
  footprint ~2.76MB vs CouchDB's ~229MB (~83x smaller — the whole reason
  this project exists); bulk-write throughput ~16.8x faster (88ms vs
  1,477ms for 5,000 docs); sequential-read latency roughly comparable
  (~1.17x faster, dominated by HTTP round-trip on both sides either way).
  Also reports the honest downside found while measuring: this server
  currently uses ~3.5x *more* disk per document than CouchDB (6.1MB vs
  1.74MB for the same 5,000 docs), likely because every write
  re-serializes a doc's entire revision tree rather than appending just
  the new revision — a real, reported trade-off, not spin, and a
  candidate for future optimization.
- Fixed the on-disk-size gap just found above (Phase 4): two changes,
  both in `db/src/storage.rs`/`db/src/main.rs`. (1) Enabled sled's zstd
  compression (`use_compression(true)`, `compression` Cargo feature) —
  off by default, and JSON compresses well; the bigger of the two wins.
  (2) Every write was doing an extra sled write for a hand-rolled
  sequence counter (`meta.update_and_fetch`) purely to generate a
  `_changes` sequence number; replaced with sled's own lock-free
  `Db::generate_id()`, and `current_seq()` now reads the sequence log's
  own highest key instead of maintaining separate, redundant state.
  Result: 6.1MB → 2.6MB for the same 5,000 docs (CouchDB: 1.74MB) — gap
  closed from ~3.5x to ~1.5x. Trade-off: write throughput dropped from
  ~56,800 to ~29,940 docs/sec (compression costs CPU) — still ~8x faster
  than CouchDB, so clearly worth it. Binary grew 2.76MB → 4.42MB (zstd
  statically linked in) — still ~52x smaller than CouchDB's 229MB
  install. Verified no regression: full unit suite (14 tests), both
  PouchDB integration tests, the load test, and the differential test
  vs. real CouchDB all still pass.
- Found and fixed a related gap while re-running the differential test:
  `DELETE /{db}` wasn't implemented at all (a bare 405, which broke the
  differential test's own cleanup step against both servers). Added it —
  matches real CouchDB, though it's not part of the replication protocol
  itself (a syncing PouchDB client never calls it); mainly useful for
  test/dev hygiene. Also gave `test/differential/run.js` a cleanup step
  it was missing since it was first written (it never deleted its own
  disposable databases on either server).
- Evaluated an external optimization proposal (`suggestions.md`) by
  actually testing each claim rather than taking it on faith. Found two
  of its five "current baseline" numbers were fabricated, not measured:
  idle memory claimed as ~15MB (actually ~30.8MB, measured via
  `Get-Process` working set/private bytes) and `_changes` latency
  claimed as ~5-10ms (never measured before now). Correcting these
  mattered because targets built on wrong baselines are either
  fake-easy or impossible for the wrong reasons.
  - Added `test/benchmark/changes_latency.js` to measure real
    `_changes` propagation latency. Full HTTP round-trip (write ACK to
    subscriber receiving it): ~13.4ms avg. Isolated in-process
    propagation only (write already ACKed to broadcast-channel
    delivery): ~0.14ms avg, p95 1ms — **already meets the "<1ms"
    target** the suggestions doc proposed a ring-buffer cache to
    achieve. The ~13ms end users would see is HTTP/TCP overhead on the
    write itself, which no `_changes`-side caching touches.
  - The proposed "<10MB active memory" target was never reachable — it
    sits below the real *idle* baseline (~30.8MB working set), let
    alone under load (~56-60MB measured with 30 concurrent `feed=continuous`
    subscribers and a 5,000-doc batch).
  - Tested the proposed "faster zstd level" fix for write-throughput
    loss directly (`COUCHDB_CLONE_COMPRESSION_LEVEL` env var, level 1
    vs. default): no measurable effect on speed or disk size. For
    documents this small, compressed independently, the effort-level
    knob doesn't have enough material to matter — fixed per-call
    overhead dominates. Left as a configurable knob (harmless) but not
    the fix it was proposed as.
  - The one genuinely well-targeted idea — binary-packing the revision
    tree instead of JSON-wrapping it — was implemented and measured: a
    `bincode`-encoded `StoredTree`/`StoredNode` (`db/src/storage.rs`),
    keeping the document body as pre-serialized raw JSON bytes since
    `bincode` can't handle `serde_json::Value` directly (hit this exact
    trap once before, see Phase 0 above). Result: 2.6MB → 2.31MB for
    the same 5,000 docs — gap vs. CouchDB (1.74MB) closed from 1.5x to
    1.33x. Cost: write throughput dropped further, ~29,940 → ~23,585
    docs/sec — still ~7.2x faster than CouchDB. Corrected a units
    mistake found while writing this up: the binary size after adding
    zstd was previously reported as 4.42MB; it's actually 4.23MB
    (~54x smaller than CouchDB's 229MB, not ~52x) — a MB-vs-MiB
    inconsistency in earlier arithmetic, not a functional change.
  - Verified none of this regressed anything: full unit suite (14
    tests), both PouchDB integration tests, the load test, and the
    differential test against real CouchDB all still pass.
  - `doc/BENCHMARKS.md` rewritten with the full v1→v2→v3 progression
    and the corrected baselines; `doc/open-questions.md` updated with
    the remaining, larger-effort disk-size levers (dictionary
    compression, tighter rev-id encoding) left for if real-scale usage
    ever shows the gap matters.
