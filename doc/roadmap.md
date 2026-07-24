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

## Phase 2 — Live replication
- [ ] `_changes` normal mode
- [ ] `_changes?feed=longpoll`
- [ ] `_changes?feed=continuous`
- [ ] `_local` checkpoint docs
- [ ] Verify `live:true` resumes correctly after restart

## Phase 3 — Hardening
- [ ] Auth (HTTP Basic, random per-install credentials)
- [ ] CORS decision (see doc/open-questions.md)
- [ ] Load/soak testing
- [ ] Differential testing vs. real CouchDB as a standing pre-release gate

## Phase 4 — Only if actually needed
- [ ] Attachments
- [ ] Mango/`_find` proxying
- [ ] `_session` cookie auth

## Status
Phase 0 and Phase 1 complete. Starting Phase 2 next: `_changes` feed
(normal, longpoll, continuous) and `_local` checkpoint round-tripping
for real `live:true` resumable sync.
