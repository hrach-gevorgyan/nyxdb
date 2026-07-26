# Roadmap

Phased plan for building a minimal, PouchDB-compatible sync server —
implementing only the CouchDB replication protocol surface a real
PouchDB client uses, not a general CouchDB replacement. See
[USAGE.md](USAGE.md) for what's actually implemented today.

## Phase 0 — Spike (done)
- [x] `PUT /{db}`, `GET /{db}`
- [x] `_bulk_docs` (no conflicts yet, single-writer, last-write-wins)
- [x] `GET/PUT /{db}/{id}` (single write path, last-write-wins)
- [x] `GET/PUT /{db}/_local/{id}` (checkpoints, needed even for one-shot push)
- [x] `_revs_diff` (minimal presence check, no possible_ancestors yet)
- [x] Prove one-shot single-direction replication against a real PouchDB client
      (`test/integration/run.js`, `db.replicate.to()`, 2 docs, passing)

## Phase 1 — Real revision trees
- [x] Revision-tree unit tests for the shapes in plan §6.1: linear
      history, conflict (equal + unequal branch depth), resolved conflict
      (deleted branch still preserved), deleted-then-recreated, deep
      branches, `_revs_diff`-style missing-revs check. All passing.
- [x] `_bulk_docs` with `new_edits:false` (accept client's own revision
      history verbatim via `_rev`/`_revisions`, idempotent, creates real
      conflicts on divergence). Verified via unit tests and manual HTTP
      test (push + conflicting push + winner/conflict check).
- [x] `_revs_diff` (now backed by `RevTree::missing`, still no
      `possible_ancestors`)
- [x] `_bulk_get` (per-item ok/error entries; missing doc, missing rev,
      and deleted-rev cases all verified over HTTP)
- [x] Winner-picking algorithm (generation + hash tiebreak) — implemented
      and unit-tested in `db/src/revtree.rs`
- [x] `_conflicts` wired into `GET /{db}/{id}?conflicts=true`. Also fixed
      a related gap while in there: a fully-deleted doc now correctly
      404s with `reason:"deleted"` instead of returning its tombstone
      body with 200.

## Phase 2 — Live replication (done)
- [x] `_changes` normal mode (`since`, `style=all_docs`, dedupe to one
      row per doc keeping the latest seq)
- [x] `_changes?feed=longpoll` (subscribes, waits for an event or
      timeout, then re-derives from storage — never trusts a single
      event in case writes coalesced during the wait)
- [x] `_changes?feed=continuous` (catch-up rows, then live rows streamed
      as newline-delimited JSON via a per-db broadcast channel)
- [x] Every write path (`put_doc`, both `_bulk_docs` modes) publishes to
      the doc's `ChangeFeed`
- [x] `_local` checkpoint docs (already existed from Phase 0, now
      exercised for real by the live sync test below)
- [x] Verified: real two-device `db.sync({live:true, retry:true})`
      converges both directions (`test/integration/live_sync.js`)
- [x] Verified: doc, `_local` checkpoint, and `_changes` sequence numbers
      all survive a server restart (manual HTTP test) — confirms
      `live:true` can resume from `since=<checkpoint>` without a full
      re-sync

## Phase 3 — Hardening (done)
- [x] Auth (HTTP Basic, random per-install credentials or
      `COUCHDB_CLONE_USER`/`COUCHDB_CLONE_PASSWORD`; required on every
      route except `GET /`; verified via curl — public root, 401 with no
      creds, 200 with correct creds, 401 with wrong creds — and both
      integration tests updated to authenticate and still passing)
- [x] CORS decision: disabled by default, opt-in per-origin via
      `COUCHDB_CLONE_CORS_ORIGINS` (no wildcard). Verified preflight
      handling with curl: allowed origin gets `access-control-allow-origin`,
      disallowed origin doesn't, and CORS correctly bypasses auth for the
      preflight itself (a preflight never carries credentials).
- [x] Load/soak testing (`test/load/run.js`, plan §6.5: large `_bulk_docs`
      batch + many concurrent `feed=continuous` subscribers). This
      caught a real bug: subscribers silently dropped changes once they
      fell behind the broadcast channel's capacity (`RecvError::Lagged`
      was treated the same as `Ok`, discarding data). Fixed by having
      the continuous stream re-catch-up from storage on lag instead of
      silently continuing. Verified up to 80 concurrent subscribers /
      15,000 docs with zero missed changes.
- [x] Differential testing vs. real CouchDB (`test/differential/run.js`,
      plan §6.2). Drives both servers with identical `new_edits:false`
      pushes (explicit shared rev ids, so the comparison isolates
      conflict/winner-picking logic rather than hash-algorithm
      differences) building a tree with a conflict, a deeper-generation
      resolution, a deletion, and a recreation. Verified against a real
      local CouchDB 3.5.2: winning rev, `_conflicts`, `_changes` content,
      and `_revs_diff` all matched exactly on first run.

## Phase 4 — Only if actually needed
- [x] **On-disk storage size** — measured 3.5x more disk usage than real
      CouchDB for identical data (`doc/BENCHMARKS.md`), which would have
      been the one clear loss in an otherwise-favorable comparison.
      Fixed two root causes: sled's zstd compression was available but
      off by default (enabled it — the bigger win); every write did an
      unnecessary extra sled write for a hand-rolled sequence counter
      (replaced with sled's own lock-free `generate_id()`, and
      `current_seq()` now reads the sequence log's own highest key
      instead of maintaining redundant state). Result: 6.1MB → 2.6MB for
      the same 5,000 docs (CouchDB: 1.74MB) — gap closed from ~3.5x to
      ~1.5x. Cost: write throughput dropped from ~56,800 to ~29,940
      docs/sec (compression is CPU work), still ~8x faster than CouchDB.
      Verified no regression: full unit suite, both PouchDB integration
      tests, load test, and differential test vs. real CouchDB all still
      pass. Found and fixed a related gap along the way: `DELETE /{db}`
      wasn't implemented at all (a 405 broke the differential test's own
      cleanup) — added, matching real CouchDB.
- [x] **Evaluated `suggestions.md`** (external optimization proposal)
      by testing every claim rather than trusting it. Two of its five
      "current" baseline numbers were fabricated (idle memory claimed
      ~15MB, actually ~30.8MB; `_changes` latency claimed ~5-10ms, never
      previously measured). Real findings: (1) full round-trip
      `_changes` latency is ~13.4ms, dominated by HTTP overhead, but
      isolated in-process propagation is already ~0.14ms — the "<1ms"
      target is already met for the mechanism that actually matters;
      (2) the proposed "<10MB active memory" target is below the real
      idle baseline and was never reachable; (3) the proposed "faster
      zstd level" fix for write throughput had no measurable effect,
      tested directly; (4) the one well-targeted idea — binary-packing
      the revision tree instead of JSON — was implemented and worked:
      2.6MB → 2.31MB, closing the CouchDB gap from 1.5x to 1.33x, at the
      cost of write throughput dropping to ~23,585 docs/sec (still
      ~7.2x faster than CouchDB). Full details and exact numbers in
      `doc/BENCHMARKS.md`. Verified no regression across the full test
      suite.
- [ ] Attachments
- [ ] Mango/`_find` proxying
- [ ] `_session` cookie auth

## Status
Phases 0 through 3 all complete, plus a Phase 4 storage-efficiency fix
(see above) that closed most of the one clear disk-usage gap found in
benchmarking. Along the way, load testing and differential testing
against real CouchDB caught/confirmed real things (a dropped-changes bug
under load, and — reassuringly — exact agreement with real CouchDB's
conflict/winner-picking behavior). Still plaintext HTTP, so keep this on
a trusted LAN until TLS is decided (doc/open-questions.md). Everything
remaining in Phase 4 (attachments, Mango, session auth) is explicitly
"only if actually needed" — no known concrete need yet, so nothing is
planned there without one. The remaining ~1.5x disk-size gap vs. CouchDB
is a known, logged candidate for further optimization (doc/open-questions.md),
not attempted here since closing it further means changing how revision
history is stored on disk — a larger, riskier change than this session's fix.
