# Roadmap

Phased plan, per [rust-couchdb-clone-plan.md](../rust-couchdb-clone-plan.md) §8.

## Phase 0 — Spike
- [x] `PUT /{db}`, `GET /{db}`
- [x] `_bulk_docs` (no conflicts yet, single-writer, last-write-wins)
- [x] `GET/PUT /{db}/{id}` (single write path, last-write-wins)
- [ ] Prove one-shot single-direction replication against a real PouchDB client

## Phase 1 — Real revision trees
- [ ] Multi-revision tracking (per-doc revision tree)
- [ ] `_revs_diff`
- [ ] `_bulk_get`
- [ ] Winner-picking algorithm (generation + hash tiebreak)
- [ ] `_conflicts` on fetch

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
Phase 0 nearly done: `PUT/GET /{db}`, `GET/PUT /{db}/{id}`, `_bulk_docs`
implemented and manually verified. Remaining: real PouchDB client test.
