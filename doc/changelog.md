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
