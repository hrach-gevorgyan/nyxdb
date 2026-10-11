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

## Phase 4 — Attachments (done)
- [x] Closed most of a disk-size gap found in benchmarking (3.5x → 1.29x
      vs. CouchDB). Also measured memory against real CouchDB — this
      server is 2-3x lighter, not a gap. Full story in `changelog.md`
      and `BENCHMARKS.md`.
- [x] Full project audit (correctness, security, stability) before
      starting attachments work — see `AUDIT.md`. Found and fixed 5 real
      issues (non-JSON error bodies in 3 more places, timing-unsafe
      credential comparison, unrestricted credentials file permissions,
      no request body size limit, panics on corrupted on-disk data).
- [x] Attachments: inline base64 (via `PUT`/`_bulk_docs`) and standalone
      upload/fetch/delete (`GET/PUT/DELETE /{db}/{id}/{attname}`).
      Content-addressed storage (dedup by digest). Verified against a
      real PouchDB client's `putAttachment`/`getAttachment` and directly
      against real CouchDB's response shape. See `USAGE.md` for the
      endpoints and the (cosmetic) differences from CouchDB's format.

## Phase 5 — Review fixes (done, v0.1.7)
- [x] A full review's CRITICAL/HIGH/MEDIUM/LOW list: concurrent-write
      branch loss, `_changes` skips, durability (flush before ack, atomic
      tree + seq), `?rev=` reads, bounded growth, md5 digests, persisted
      server id, longpoll races, startup/memory/blocking, body limit,
      write/read surface (`DELETE`, `_all_docs`, `_design/…`), attachment
      compaction, request limits. Each one test-first; see `changelog.md`.
- [x] Integration suite runs on PouchDB 8.0.1 and 9.x.

Mango/`_find` proxying and `_session` cookie auth are dropped from
scope — no concrete need, not planned.

## Next
- Move attachment bytes out of sled (its cache cap doesn't bound ~1 MB
  values; see `USAGE.md` §3) — an open option, not started.
- Keep testing against the real target app (Offlog).

## Status
Phases 0–5 done. Verified with a real PouchDB client, load testing,
differential testing against real CouchDB, ported PouchDB test cases,
and a full security/stability audit. Plaintext HTTP only — keep this on
a trusted LAN until TLS is decided (`open-questions.md`).
