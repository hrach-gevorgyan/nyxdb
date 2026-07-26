# Roadmap

Building a minimal, PouchDB-compatible sync server — only the
replication protocol surface a real PouchDB client uses, not a full
CouchDB replacement. See [USAGE.md](USAGE.md) for what's implemented
today, [changelog.md](changelog.md) for how/why things changed.

## Phase 0 — Spike (done)
- [x] `PUT/GET /{db}`, `GET/PUT /{db}/{id}`
- [x] `_bulk_docs` (single-writer, last-write-wins)
- [x] `_local` checkpoints, `_revs_diff` (basic)
- [x] One-shot replication verified against a real PouchDB client

## Phase 1 — Real revision trees (done)
- [x] Revision-tree unit tests (linear history, conflicts, deletion,
      recreation, deep branches) — 14 tests, `db/src/revtree.rs`
- [x] `_bulk_docs` with `new_edits:false` (real conflict creation)
- [x] `_bulk_get`
- [x] Winner-picking (generation + hash tiebreak)
- [x] `_conflicts` on `GET /{db}/{id}?conflicts=true`

## Phase 2 — Live replication (done)
- [x] `_changes`: normal, `feed=longpoll`, `feed=continuous`
- [x] Every write publishes to a per-db change feed
- [x] Verified: two-device `live:true` sync converges, resumes after restart

## Phase 3 — Hardening (done)
- [x] HTTP Basic auth, required everywhere except `GET /`
- [x] CORS: off by default, opt-in per-origin
- [x] Load testing (`test/load/`) — caught and fixed a dropped-changes
      bug under load
- [x] Differential testing against real CouchDB (`test/differential/`)
      — matches exactly

## Phase 4 — Only if actually needed
- [x] Closed most of a disk-size gap found in benchmarking (3.5x → 1.29x
      vs. CouchDB). Also measured memory against real CouchDB — this
      server is 2-3x lighter, not a gap. Full story in `changelog.md`
      and `BENCHMARKS.md`.
- [ ] Attachments
- [ ] Mango/`_find` proxying
- [ ] `_session` cookie auth

Nothing below Phase 3 is planned without a concrete need — see
`open-questions.md`.

## Status
Phases 0–3 done. Verified with a real PouchDB client, load testing, and
differential testing against real CouchDB. Plaintext HTTP only — keep
this on a trusted LAN until TLS is decided.
