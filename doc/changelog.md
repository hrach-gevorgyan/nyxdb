# Changelog

> Renamed from "couchdb-clone" to **NyxDB**. Entries below predate the
> rename and refer to the project by its old working name and old
> `COUCHDB_CLONE_*` env var prefix (now `NYXDB_*`) — left as an honest
> historical record rather than rewritten.

## v0.1.7

Fixes from a read-only review of v0.1.6, in its priority order. Each
fix was written test-first: the new test failed before the fix and
failed again with the fix deliberately broken.

The PouchDB integration suite (and `test/write_semantics`) now also runs
on PouchDB 9.x, what Offlog ships, as well as 8.0.1: `POUCHDB=9`, a
second CI step. All of it passes on 9.0.0, and it catches the v0.1.6
`_revisions` bug there too when that is reintroduced.

**1. Concurrent writes to the same doc lost a branch (critical).**
Every write did read tree → modify → write tree with no lock, on a
multi-threaded runtime. Two simultaneous writes to one doc both read
the same tree and the second overwrote the first; both clients got a
success response and the lost branch was never re-sent. Reproduced
with 24 parallel writers to one doc: ~21 of 24 acknowledged revisions
lost, on all three write paths (`PUT ?new_edits=false`, `_bulk_docs
new_edits:false`, plain `PUT`). Fixed with a per-database write lock:
`Db::update_tree` is now the only way a document gets written, and
holds the lock across read → modify → write → seq assignment →
publish. Test: `test/concurrency/run.js`.

**2. `_changes` could skip changes forever (high).** (a) The normal
feed collected its results, then read the db's current seq as
`last_seq`; a write landing in between sat below the client's new
checkpoint without ever being returned. Reproduced: a follower polling
by `last_seq` during 16 parallel writers permanently missed ~50 of 960
docs. `last_seq` is now the highest seq the scan actually covered (or
`since`). (b) Seqs were assigned and published outside any lock, so
they could become visible and be broadcast out of order, and the
continuous feed drops `seq <= last_seq` — fixed by the #1 lock, which
now covers seq assignment and publishing. Test:
`test/changes_follow/run.js` (polling + continuous followers); breaking
(a) makes the poller miss docs, breaking (b) makes the continuous
follower miss docs.

**3. Acknowledged writes could be lost on a hard kill (high).** sled
flushes to disk only every ~500 ms in the background, no handler
flushed, and Offlog stops NyxDB with TerminateProcess, so no shutdown
hook ever ran. Reproduced by hard-killing the server right after
acknowledged writes: dozens of acknowledged docs and every round's
last acknowledged `_local` checkpoint were gone after restart. Now:

- Every write handler (`PUT`, `_bulk_docs`, `_local`, attachment
  `PUT`/`DELETE`) flushes to disk before acknowledging — once per
  request, so a `_bulk_docs` batch costs one flush, not one per doc.
- A doc's tree and its `_changes` entry are written in one sled
  transaction, so a crash can't keep one without the other (a tree
  with no seq entry would make `_revs_diff` say "have it" while
  `_changes` never lists it — nobody would ever re-send or pull it).
- Ctrl-C, Ctrl-Break, console close and system shutdown (SIGTERM on
  Unix) flush and stop cleanly, with a 3 s grace period for open
  connections (live `feed=continuous` clients never close on their own).

Test: `test/durability/run.js` spawns its own server, writes as fast as
it can, hard-kills it immediately after an acknowledgement, restarts,
and checks every acknowledged doc and checkpoint. Fails before the fix
and with `Db::flush` made a no-op. The signal-driven shutdown path
isn't covered by an automated test (Windows console signals can't be
sent to a background child process); a hard kill is the case that
matters for Offlog and is what the test exercises.

**4. Attachment reads ignored `?rev=` (high).** `GET /db/doc/att` and
`GET /db/doc` always used the winner. A pulling PouchDB fetches each
attachment with the rev of the branch it belongs to, so a losing
branch's attachment either 404'd (and the batch retried forever) or
got the winner's bytes. Reproduced: two conflicting branches with
different bytes under the same name, replicated into a second PouchDB —
both branches ended up with the winner's bytes. Both routes now honour
`rev`, with `404 missing` for an unknown one. Test: added to
`test/attachments/run.js`.

**5. Unbounded growth (high).** Every revision kept its full body
forever, every write re-serialised the whole tree, and every write
appended a `_changes` entry that was never removed. Measured: a doc
with a 2 KB body edited 1,500 times grew from 20 KB to 3.1 MB stored,
and the test took ~12 s (each write rewriting everything before it).
Now, on every write:

- only leaves keep a body (like CouchDB after compaction); ancestors
  keep id and parent link, so history and `_revs_diff` still work;
- each branch is stemmed to its newest 1000 revisions (`revs_limit`);
- the doc's previous `_changes` entry is removed in the same
  transaction (a new doc → seq index), so `_changes` holds one entry per
  doc and `since=0` scans are bounded by the doc count.

Same test after the fix: tree stays bounded, one `_changes` entry,
0.66 s. Existing databases are migrated once on open (duplicate
`_changes` entries collapsed to each doc's newest); existing trees are
pruned the next time each doc is written.

Client-visible consequence: a non-leaf revision is no longer readable —
`GET ?rev=`, `open_revs`, and `_bulk_get` without `latest` report it
`missing`, and `revs_info` lists ancestors as `missing` (this also fixes
the v0.1.6 known gap where empty ancestor placeholders reported
`available`). Replication only reads leaves, and PouchDB always sends
`latest=true`. Tests: `long_history_stays_bounded`,
`opening_a_pre_index_database_migrates_its_seq_log` (storage),
`prune_*` (revtree), and long-history/non-leaf checks in
`test/revisions/run.js`.

**6. The server uuid changed on every start (medium).** `GET /`'s
`uuid` was a fresh random value per process. PouchDB builds its
replication id from it, so after every restart every client had a new
replication id, found no checkpoint, and re-synced from `since=0`.
Reproduced with a real PouchDB pull across a restart: the second pull
asked for `_changes?since=0`. The uuid is now generated once and stored
with the data. Tests: `test/integration/checkpoint_resume.js` (spawns
its own server) and `server_uuid_survives_a_restart` (storage).

**7. Attachment digests didn't match PouchDB's, so every pull
re-downloaded every attachment (medium).** Stubs carried `sha256-<hex>`
digests. PouchDB's replicator (`fileHasChanged`) compares the server's
digest with the `md5-<base64>` digest it computed for its own copy, so
they never matched. Reproduced: pulling an edit that only changed the
doc body still downloaded its attachment. Stubs now carry CouchDB-style
`md5-<base64>` digests; bytes stay content-addressed by sha256
internally, via an md5 → sha256 index. Stubs written by older versions
still resolve and are shown as md5. Adds the `md-5` crate (RustCrypto,
same family as the existing `sha2`).

Also fixed here (from the LOW list, same code path): a stub whose digest
the server doesn't have was stored anyway, leaving an attachment that
could never be read — now `412 missing_stub` (per-doc `missing_stub` in
`_bulk_docs`). And a plain `PUT` re-sending a bare `{"stub":true}` for an
unchanged attachment stored a digest-less stub that made the attachment
`GET` fail with a 500 — it now keeps the previous revision's attachment.

Tests: md5 digests on every read path, "no re-download on an
attachment-preserving edit" (real PouchDB pull, counting attachment
GETs), missing stub → 412, bare stub kept — in
`test/attachments/run.js`; legacy sha256 stub resolution as a unit test.

**8. Every route created the database it named (medium).** Opening a
db created its storage trees, so `GET /typo`, `PUT /typo/doc`,
`_changes`, `_bulk_docs`… all quietly created `typo` — a mistyped db name
in a sync URL produced an empty database and a "successful" sync into
it. Now only `PUT /{db}` creates; everything else returns CouchDB's
`404 "Database does not exist."`. Names are validated with CouchDB's
rule (which also rejects `_users`/`_replicator` and anything with `:`,
the separator in this server's storage namespace). Test:
`test/databases/run.js` (11 routes on a missing db, 6 invalid names, one
valid name using every allowed character class).

**9. Longpoll could miss a write, and ignored `heartbeat` (medium).**
Longpoll read the current seq, then subscribed; a write landing in
between was missed and the client waited out the full timeout (60 s by
default) for a change that had already happened. It now subscribes
first, then checks storage. That race is microseconds wide, so it's
pinned by a deterministic unit test that lands a write exactly in the
gap (`longpoll_sees_a_write_landing_right_after_its_storage_check`) —
it times out with the old order and passes with the new one.

`heartbeat` was ignored on both longpoll and continuous feeds. PouchDB's
live sync sends `feed=longpoll&heartbeat=10000` and aborts a request
after heartbeat + 5 s of silence, so every idle longpoll was being cut
off by the client and retried. Both feeds now write a newline every
`heartbeat` ms while idle; on longpoll, as in CouchDB, a heartbeat
overrides `timeout` and the eventual body is ordinary JSON after the
newlines. `timeout` keeps working as before. Test: `test/feeds/run.js`
(heartbeat on both feeds, the final body after heartbeats, `timeout`,
prompt wake-up on a write).

**10. sled resource limits (medium).** Three parts:

- *Startup failures panicked* (exit 101, "thread 'main' panicked",
  backtrace hint) for every cause alike. They now print one line and
  exit 2 (bad setting), 3 (data directory unusable) or 4 (can't listen),
  so Offlog can tell the user why its sidecar didn't start. Invalid
  `NYXDB_MAX_BODY_BYTES` used to be silently ignored; it's now exit 2
  too. Test: `test/startup/run.js`.
- *Memory grew with the data.* sled's page cache defaults to 1 GB.
  Measured with 100 MB of 2 KB docs: ~195 MB resident after writing,
  ~163 MB after reading it back in a fresh process. Capped at 32 MB by
  default (`NYXDB_CACHE_MB`): ~105 MB. The review suggested 32–64 MB;
  64 MB measured ~150 MB, because sled undercounts real memory by about
  2×, so 32 it is. Test: `test/memory/run.js` (spawns its own server;
  fails at sled's old default). **Known limit, measured and documented
  rather than fixed:** in sled 0.34 the cap does not bound large values
  (~1 MB each) — 200 MB of 1 MB docs reaches ~230 MB resident with an
  8 MB or a 1 GB cap alike. The real fix is storing attachment bytes
  outside sled; left as an open decision.
- *Slow requests stalled everything.* sled calls ran on tokio's async
  workers (one per CPU), so a few full `_changes` scans of a large db
  stalled even `GET /` (measured: 1.3–1.5 s). Storage work in every
  handler now runs via `block_in_place`, which hands the worker's other
  tasks to another thread. Test: `test/blocking/run.js` (80 scans of a
  20k-doc db while timing `GET /`; 1,025 ms with the fix removed).

**11. The 50 MB body limit could stall sync (medium).** PouchDB pushes
100 docs per `_bulk_docs` request with attachments inline as base64.
Reproduced: a real PouchDB push of 120 docs with 600 KB attachments
(~80 MB per batch) got `413` and failed. The default limit is now
256 MB — one maximal Offlog doc (ten 10 MB attachments, ≈133 MB as
base64) fits — and an over-limit body gets CouchDB's
`413 {"error":"too_large"}` instead of a generic `bad_request`. Not
streamed: a request is still held in memory while processed, so the
limit is also the memory guard. `doc/USAGE.md` spells out the
`batch_size` rule for clients (Offlog needs `batch_size: 1` for
attachment-heavy replication, or a higher server limit). Test:
`test/integration/large_batches.js` (250-doc batch, the PouchDB push,
just-under and just-over a small limit on a self-spawned server).

**LOW: write semantics.**

- `DELETE /{db}/{id}` didn't exist (405). That includes the request
  PouchDB's `remote.remove(id, losingRev)` sends to resolve a conflict
  on a remote — reproduced with a real PouchDB client. Now a tombstone
  on top of `rev` (must be a current leaf, else 409), checked inside the
  write lock via a new `Db::try_update_tree`.
- `_deleted:true` on `PUT`/`_bulk_docs` was stored as an ordinary field
  on a live revision; it now deletes. A write naming a current losing
  leaf's `_rev` now extends that leaf instead of the winner (still no
  optimistic concurrency: a stale `_rev` falls back to the winner).
- `PUT` stored the client's stale `_rev` inside the new revision's body.
  `_id`/`_rev`/`_deleted` are now stripped (unit test — it isn't visible
  over HTTP).
- The rev hash now covers the parent and deleted flag, like CouchDB's;
  otherwise two empty-body tombstones on different branches at the same
  generation got the same rev id.
- `_local` docs reported rev `0-1` forever and couldn't be deleted. They
  now get `0-N` (incremented atomically) and have `DELETE`.

Test: `test/write_semantics/run.js` (all 9 checks fail before the fix).
And `test/integration/delete_replication.js`: a deletion made on one
PouchDB client, and one made with `DELETE` on the server, reach clients
that already hold the doc as a single deleted leaf with full history (the
server-side half failed before, with a 405).

**LOW: read endpoints.**

- `GET /{db}/_all_docs` (404 before): live docs in id order, tombstones
  left out, with `include_docs`, inclusive JSON `startkey`/`endkey`,
  `descending`, `skip`, `limit`, `total_rows`, `offset`. `POST` with
  `{"keys": [...]}` answers in request order, `error: not_found` for
  unknown ids and `value.deleted: true` for deleted ones.
- `PUT /{db}/_design/app` was stored as id `_design` (the `/` split the
  path). `/{db}/_design/{name}` now has GET/PUT/DELETE on id
  `_design/{name}`. Design docs are stored only; NyxDB runs no views.
- `GET /{db}` counted tombstones in `doc_count`. Now `doc_count` is live
  docs, plus `doc_del_count`, `update_seq` and `instance_start_time`.
- `_changes?limit=N` was ignored. It now returns at most N rows, and
  `last_seq` is the last returned row's seq, so the next page resumes
  there.

Test: `test/read_surface/run.js` (all 7 checks fail before the fix).

**LOW: storage errors in `_changes`; attachment garbage collection.**

- `_changes` silently dropped a row whose stored tree couldn't be read,
  and the client's checkpoint then moved past that change for good. Now
  the normal/longpoll response is a 500 and a continuous feed ends before
  the row (the client reconnects from the last seq it got); the error is
  logged. Unit test: `changes_reports_an_unreadable_doc_instead_of_dropping_it`.
- Attachment bytes were never deleted, even after every revision using
  them was replaced or pruned. New `POST /{db}/_compact` (202) deletes
  bytes no stored revision refers to, and their md5 index entries.
  Writes that store or check attachment bytes hold a per-database gate
  until their revision is committed; compaction takes it exclusively, so
  it can't collect bytes a write has stored but not yet referenced.
  Tests: unit `compaction_collects_only_unreferenced_attachments`;
  `test/attachments/run.js` (replaced bytes gone, current and
  conflict-branch bytes kept).

**LOW: operational limits.**

- No request timeout: a client that stalled mid-upload held its buffer
  and a task forever. `NYXDB_REQUEST_TIMEOUT_SECS` (default 300, `0` =
  off) answers `408 request_timeout`, counting the body read.
- No limit on concurrent work. `NYXDB_MAX_CONCURRENT_REQUESTS` (default
  512) caps requests in progress; more wait for a slot. axum 0.7's
  `serve` can't cap raw connections, so idle connections aren't limited.
- `_changes` is exempt from both: longpoll waits by design (its own
  `timeout`), continuous feeds never end, and neither should take a slot.
- No request logging. `TraceLayer` added, and `RUST_LOG` now works
  (default `info`; `info,tower_http=debug` logs each request).
- CORS preflights had no `max-age`, so a browser client re-sent one
  before every write. Now cached for an hour.

Test: `test/ops/run.js` (own server, tiny limits; each part
break-tested separately).

## v0.1.6

**Real replication bug: every edit of an already-synced doc became a
phantom conflict on the other replica.** NyxDB stored revision trees
correctly (`_revs_diff` knew old revs, `_changes?style=all_docs` showed
one leaf), but no read path ever returned a document's history:

- `POST /{db}/_bulk_get?revs=true&latest=true` — what PouchDB uses to
  pull changes — returned `{"ok": doc}` without `_revisions`, whether
  `revs` came in the query string or the body.
- `GET /{db}/{id}` ignored `revs=true` and `revs_info=true`.
- `GET /{db}/{id}?open_revs=...` returned a plain doc instead of
  CouchDB's array of `{"ok": doc}` / `{"missing": rev}`.

Without `_revisions`, a pulling PouchDB stores the incoming revision
(`new_edits:false`) as a new root beside the copy it already has. A
first full sync (every doc at rev 1) has no history to lose, which is
why the existing tests never caught it — it needs a doc that's already
on both replicas and then gets edited.

Fixed: `revtree.rs` now exposes the ancestry walk it already had the
data for (`ancestry`, `revisions`, `leaves_descending_from`), and
`routes.rs` builds every read path's body through one shared function
(`revision_body`). `_bulk_get` honours `revs`/`latest` from the query
string or the body; `GET` honours `revs`, `revs_info`, `open_revs`
(`all` or a JSON list) and `latest`, in CouchDB's documented JSON
shapes. The v0.1.4 tombstone behaviour (`ok` with `_deleted:true`,
never a `deleted` error) and the v0.1.3 `PUT ?new_edits=false`
handling are unchanged — both still pinned by `test/ported/`.

Tests written first and confirmed failing before the fix, then
confirmed failing again with the fix removed:
- `test/integration/edit_replication.js` — two live PouchDB replicas,
  create on A, two edits on A, read on B with `{conflicts, revs}`.
  Before: B had `_conflicts` and one-entry `_revisions`. After: no
  conflicts, full three-rev history.
- `test/revisions/run.js` — 16 endpoint checks for the shapes above,
  on a fixed revision tree built with `PUT ?new_edits=false`.
- 4 new `revtree.rs` unit tests for the ancestry walk (24 total).

Known gap, documented in `doc/USAGE.md`: ancestors that only ever
arrived as ids in a pushed `_revisions` chain are stored as empty
placeholders, so `revs_info` calls them `available` and `open_revs`
returns them with an empty body; real CouchDB would say `missing`.
Replication never reads ancestor bodies, so sync isn't affected.

Not re-run for this release: the differential test and
`doc/BENCHMARKS.md`, both of which need a live real CouchDB instance
that wasn't available. `BENCHMARKS.md` still reflects v0.1.5; nothing
in this change touches the write path or storage format.

## v0.1.5

**Follow-up investigation** of a reported anomaly: `_changes` sequence
numbers jumping by roughly 2,000,000 around conflicted-document
writes, flagged as a possible unresolved side effect of the same code
path behind the v0.1.4 `_bulk_get` bug. Investigated directly rather
than assumed either way — the report explicitly asked whether this was
"a deliberate, bounded design... or an unintended side effect."

**Found two separate things while investigating:**

1. **The reported jump itself is not a bug.** Traced to sled's own
   source (`idgen_persist_interval`, default 1,000,000): `generate_id()`
   increments by exactly 1 per call during normal operation — it never
   jumps in value. On database *reopen*, though, sled deliberately
   resumes counting from beyond its last-persisted checkpoint (not the
   true last-used value) so it can never reuse an id after an unclean
   shutdown — confirmed directly by restarting a real instance against
   the same data directory and watching the next `seq` land at
   2,000,001 instead of continuing from 8. The correlation with
   "conflicted batches" in the original report was coincidental,
   not causal — pairing two devices for the first time plausibly
   happens right after starting/restarting the embedded sidecar
   process, which is what was actually triggering it. Documented in
   `doc/USAGE.md` so this doesn't get re-reported as a mystery.

2. **A real, more serious bug found as a side effect of checking the
   first hypothesis**: `generate_id()` starts at 0, but `since=0`
   (what every fresh `db.sync()` starts with) means "from the very
   beginning" — `changes_since` correctly treats `since` as exclusive
   (`seq > since`), which made the **very first document ever written
   to a fresh database permanently invisible** to any client's initial
   sync. Confirmed directly: a fresh database with exactly one document
   reported `doc_count:1` but `_changes` returned zero results. This
   is a more universal bug than the seq-jump report itself (it hits
   every single fresh database's first write, not just conflicted
   ones) and had gone unnoticed because most testing so far seeded
   multiple documents at once, masking it.

   Fixed by shifting persisted `seq` values into `1..=u64::MAX` (never
   0), so `since=0` unambiguously means "nothing seen yet" and every
   real write is always `> 0`. Added a direct unit test in
   `db/src/storage.rs` (the project's first storage-layer test,
   previously only `revtree.rs` had unit tests) using sled's
   `temporary(true)` mode rather than pulling in a `tempfile`
   dependency for one test. Confirmed it fails against the pre-fix
   code and passes against the fix.

Full existing suite re-verified: 20 unit tests (up from 19), both
PouchDB integration tests, all ported cases, attachments, load, and
the differential test against real CouchDB — all pass. Benchmarks
re-run and `doc/BENCHMARKS.md` updated (see that file for the date,
time, and version tested).

## v0.1.4

**Real bug found live-testing against a real PouchDB app** (offlog-app
+ offlog-desktop's embedded NyxDB sidecar), reproduced independently
twice, including once on a completely fresh install with zero prior
state: `_bulk_get` returned `{"error":{"reason":"deleted"}}` for a
revision that genuinely exists in the tree but happens to be a
tombstone — a losing conflict branch, or a fully-deleted document's
resolved winner — instead of returning it as a normal `ok` result with
`_deleted:true` embedded in the body.

Confirmed directly against real CouchDB 3.5.2 (not assumed) that it
always returns `"ok"` with `_deleted:true` for any revision that
exists in the tree, whether requested explicitly by rev or resolved
via the current winner, and never uses the `"reason":"deleted"` error
shape at all — that error path doesn't exist for `_bulk_get` in real
CouchDB. This mismatch broke real PouchDB replication end to end:
`getDocs()`/`bulkGet()` (`pouchdb.js`) treats any `doc.error` entry in
a batch as a hard failure and aborts the whole sync with a generic
"There was a problem getting docs." error — so any sync touching an
unresolved multi-way conflict failed permanently. The most common
real-world trigger: two independently-seeded devices pairing for the
first time, each having created the same fixed-ID default documents
locally before ever syncing — exactly offlog-app's own
`seedIfEmpty()`/`clearLocalSeedBeforeFirstPair()` scenario.

Investigated an initial lead (a suspicious jump in `_changes` sequence
numbers, landing in the 2,000,000 range for the affected documents)
and ruled it out directly: that's sled's own `generate_id()` batch-
reservation behavior, not a bug — sequence numbers are already
documented as opaque and non-portable between servers (`doc/USAGE.md`
§7), so a jump in the *value* was a red herring, not the actual defect.
The real bug was purely in how `bulk_get` (`db/src/routes.rs`) mapped
a tombstoned tree node to a response, independent of the revision-tree
insertion logic itself (already correct, per the passing differential
test and `revtree::tests::*`).

Fixed by returning the tombstone's body as an `ok` result with
`_deleted:true` set, for both the explicit-rev and resolved-winner
cases — plain `GET /{db}/{id}` (no `rev`) deliberately keeps its
existing, different 404-on-deleted-winner behavior, since "does this
document currently exist" and "give me this specific revision's
content" are genuinely different questions, and real CouchDB answers
them differently too (verified directly, both ways, before fixing).

Added two regression tests (`test/ported/pouchdb_tests.js`): the
explicit-rev case and the no-rev/deleted-winner case, plus a guard
test confirming plain `GET` still 404s correctly (so the fix doesn't
overcorrect). Confirmed both new tests fail against the pre-fix code
and pass against the fix. Full existing suite re-verified: 19 unit
tests, both PouchDB integration tests, all ported cases (13 total now),
attachments, load test, and the differential test against real
CouchDB — all pass.

Benchmarks re-run and `doc/BENCHMARKS.md` updated (see that file for
the full numbers and a note on this run's write-throughput variance,
which is machine noise, not a regression — nothing in this release
touches the write path).

## v0.1.3

**Real bug found live-testing against a real PouchDB app** (Offlog):
a single `PUT /{db}/{id}?new_edits=false` with an explicit `_rev` at a
generation that already exists silently applied as a normal edit
instead of forking a real conflict — `put_doc` ignored `?new_edits=`
entirely and always took the last-write-wins path, discarding the
client-supplied `_rev`. The equivalent operation via `_bulk_docs` with
`new_edits:false` already worked correctly, which is what made this a
routing bug in `db/src/routes.rs`, not a rev-tree bug — the underlying
conflict/winner-picking logic (already covered by the
`pouchdb_conflict_resolution_*` unit tests and the differential test)
was never wrong.

Fixed by having `put_doc` branch on `?new_edits=` the same way
`_bulk_docs` already does, and extracting the shared tree-insertion
logic (`write_doc_new_edits_false`) so both paths use the exact same
code instead of two implementations that could drift apart again.
Verified against real CouchDB 3.5.2 directly (not just our own
expectations) that a diverging single-PUT `new_edits=false` creates a
genuine sibling conflict there too — this server now matches. Added a
regression test (`test/ported/pouchdb_tests.js`) that fails against
the old code and passes against the fix, confirmed both ways before
merging. Full existing suite (19 unit tests, both PouchDB integration
tests, ported cases, attachments, load, differential vs. real
CouchDB) still passes.

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
- Closed the two deferred audit findings with proper fixes instead of
  leaving them open: added `axum::extract::DefaultBodyLimit` (50MB
  default, `COUCHDB_CLONE_MAX_BODY_BYTES` to override) so an oversized
  request gets a clean `413` instead of unbounded memory buffering.
  Converted every panic-on-corrupt-data path in `storage.rs` to a
  proper `StorageError`/`StorageResult` chain instead of `.expect()`.
  Verified: oversized bodies correctly rejected with a JSON error, full
  regression suite (19 unit tests, integration, differential, load,
  ported tests) still passes.
- Fixed the remaining audit "review" items instead of leaving them
  open: `prod/Dockerfile` now creates and runs as a dedicated non-root
  user; `Credentials` no longer derives `Debug` (manual impl redacts
  the password).
- Closed every item in `open-questions.md` — most by deciding and
  documenting the reasoning (plaintext HTTP is deliberate for a
  trusted-LAN deployment; rate limiting and connection caps aren't
  needed for that same model), a few by the fixes above.

**Phase 4 — Attachments.** Two ways to attach a file, matching what a
real PouchDB client actually sends:
- Inline base64 in the document body (`_attachments.<name>.data`),
  supported on `PUT`, `_bulk_docs`, and `new_edits:false`. Extracted on
  write into a stub (`{"content_type","digest","length","stub":true}`)
  plus separately-stored bytes — keeps the revision-tree storage small
  regardless of attachment size. `GET` returns stubs by default,
  `?attachments=true` re-inflates them to inline base64.
- Standalone `GET/PUT/DELETE /{db}/{id}/{attname}` — raw bytes in/out,
  `Content-Type` from the request/response header, not JSON-wrapped.
- Storage is content-addressed by digest (`db/src/attachments.rs`,
  new `attachments` sled tree per db) — identical attachment content is
  only ever stored once, even across documents or revisions.
- Verified three ways: our own HTTP-level tests (`test/attachments/`),
  a real PouchDB client's `putAttachment`/`getAttachment` methods (same
  file), and a direct comparison against real CouchDB's actual response
  shape — matches when the client sends `Accept: application/json`
  (which PouchDB always does). Two cosmetic differences found and
  documented, not fixed: digests use SHA-256 not CouchDB's MD5 (same
  reasoning as the revision-hash difference), and stubs omit `revpos`.
  No `multipart/related` support — inline JSON only.
- Mango/`_find` proxying and `_session` cookie auth, the other two
  originally-listed Phase 4 candidates, dropped from scope entirely —
  no concrete need arose.

Added `doc/MIGRATING.md`: instructions for pointing an existing
PouchDB app (currently synced against real CouchDB) at this server —
compatibility checklist, deployment steps, data migration via normal
replication (no separate migration tooling needed, since each device's
local PouchDB already has a full copy), a test-before-cutover
checklist, and a rollback note (switching the remote URL back is the
entire rollback).

This closes out the full arc from `doc/roadmap.md`: Phases 0 through 4
all complete, verified by unit tests, a real PouchDB client, load
testing, differential testing against real CouchDB, ported PouchDB
test cases, and a full security/stability audit.
