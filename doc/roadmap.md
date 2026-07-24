# Roadmap

Phased plan, per [rust-couchdb-clone-plan.md](../rust-couchdb-clone-plan.md) §8.

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

## Phase 3 — Hardening
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
- [ ] Differential testing vs. real CouchDB as a standing pre-release gate

## Phase 4 — Only if actually needed
- [ ] Attachments
- [ ] Mango/`_find` proxying
- [ ] `_session` cookie auth

## Status
Phase 0, 1, and 2 complete. Phase 3: auth, CORS, and load/soak testing
done — the last of which caught and fixed a real correctness bug
(silently dropped changes under load, see changelog). Still plaintext
HTTP, so keep it on a trusted LAN until TLS is decided
(doc/open-questions.md). Remaining: differential testing vs. real
CouchDB.
