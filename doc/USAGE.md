# Usage Reference

How to run this server, every endpoint it implements, its config, and
where it differs from real CouchDB. If something here disagrees with
the code, trust the code — check `db/src/routes/` (`build_router` in `mod.rs` has the route table).

---

## 1. What this is

A Rust server implementing the part of CouchDB's HTTP replication
protocol that a real PouchDB client (`db.sync()`, `db.replicate.to/from()`)
actually uses, plus attachments. Not a full CouchDB replacement — no
Mango queries, no MapReduce views, no clustering.

See [roadmap.md](roadmap.md) for what's done and [changelog.md](changelog.md)
for why decisions were made.

**Status**: Phases 0–4 done (spike, revision trees, live replication,
hardening, attachments). Verified with a real PouchDB client, a load test, and a
differential test against real CouchDB 3.5.2 — see §5.

## 2. Running it

```bash
cargo run --manifest-path db/Cargo.toml
```

Binds `127.0.0.1:5984`, stores data under `./data`. Both configurable —
see §3.

On first run it generates HTTP Basic auth credentials and logs the
username (password is in `<data dir>/credentials.json`, or set
`NYXDB_USER`/`NYXDB_PASSWORD` yourself). Every request
needs auth except `GET /`.

```bash
curl http://127.0.0.1:5984/                                   # no auth
curl -u admin:<password> -X PUT http://127.0.0.1:5984/mydb     # needs auth
```

## 3. Configuration

| Variable | Default | Purpose |
|---|---|---|
| `NYXDB_ADDR` | `127.0.0.1:5984` | bind address |
| `NYXDB_DATA` | `./data` | data directory (also holds `credentials.json`) |
| `NYXDB_USER` | generated | admin username |
| `NYXDB_PASSWORD` | generated | admin password (set together with the username above) |
| `NYXDB_CORS_ORIGINS` | unset (CORS off) | comma-separated allowed origins, no wildcard |
| `NYXDB_CACHE_MB` | `32` | sled page-cache size; real memory is roughly 25 MB + 2× this (see below) |
| `NYXDB_COMPRESSION_LEVEL` | sled's default (5) | zstd level, 1–22 |
| `NYXDB_ATTACHMENT_GRACE_SECS` | `3600` | an attachment file younger than this is never deleted by a sweep (it may belong to an upload whose revision isn't committed yet) |
| `NYXDB_MAX_BODY_BYTES` | `268435456` (256 MB) | largest accepted request body; larger gets `413 {"error":"too_large"}` |
| `NYXDB_REQUEST_TIMEOUT_SECS` | `300` | a request (including reading its body) that takes longer gets `408 {"error":"request_timeout"}`; `0` = no limit. `_changes` is exempt |
| `NYXDB_MAX_CONCURRENT_REQUESTS` | `512` | requests in progress at once; more wait for a slot. Authenticated `GET …/_changes` is exempt (from this and the request timeout) |
| `NYXDB_HEADER_TIMEOUT_SECS` | `30` | a request's headers must arrive in full within this, counted from connecting (or from the first byte of a later request on the same connection); else the connection is closed |
| `NYXDB_IDLE_TIMEOUT_SECS` | `120` | a keep-alive connection with no request in progress and no traffic for this long is closed. A request in progress — including a longpoll or continuous feed, heartbeat or not — is never cut |
| `NYXDB_MAX_CONNECTIONS` | `256` | open connections at once; more are closed as soon as they connect. Accepted connections also get TCP keepalive (probes after 60 s of silence), so a vanished client is noticed |
| `RUST_LOG` | `info` | log filter; `info,tower_http=debug` logs every request and its status and latency |

Source: `db/src/main.rs`, `db/src/auth.rs`.

**Startup failures** print one line to stderr and exit with a code you
can act on, instead of panicking: `2` = a setting above is invalid
(named in the message), `3` = the data directory or its store can't be
opened or is damaged, or `credentials.json` exists but can't be read
(the file is left as it was, never replaced), `4` = can't listen on
`NYXDB_ADDR` (e.g. port already in use). A store counts as damaged when
sled reports an error or panics opening it, or when it existed before
but opens with no database and no server id (sled silently recovers
some damaged files as an empty store; NyxDB refuses to start on one
rather than serve it). Damage that only cuts off the most recent writes
can't be told apart from a crash and isn't detected.

**Memory.** Measured with 100 MB of small (2 KB) docs: ~105 MB resident
at the default cache size, versus ~195 MB with sled's own 1 GB default.
Attachment bytes don't go through sled's cache at all (they are files,
see Attachments): 1 GB of attachments written and read back leaves
memory flat at ~30 MB. Large *documents* (~1 MB of JSON each) still do,
and sled 0.34's cache cap doesn't bound values that size.

Storage work runs on tokio's blocking pool, so slow requests (a full
`_changes` scan of a large db) don't stall unrelated ones.

**Request size and attachments.** PouchDB pushes 100 docs per
`_bulk_docs` request by default, with attachments inline as base64
(~1.33× their size). One request must fit under `NYXDB_MAX_BODY_BYTES`
(256 MB by default), or it gets `413 too_large` — and a replicator that
keeps retrying the same batch never gets past it. Rule of thumb for a
client: `batch_size × (largest doc's attachments × 1.33)` must stay
under the limit. For Offlog (up to ten 10 MB attachments per task,
≈133 MB base64 per doc at worst), one maximal doc fits; replicating
attachment-heavy tasks needs PouchDB's `batch_size` at 1 (or the
server's limit raised). The server holds a request in memory while
processing it, so peak memory per request is a small multiple of its
size.

## 4. Endpoints

Auth: HTTP Basic on everything except `GET /`. Bodies are JSON.

### `GET /`
Server info. No auth needed.

```bash
curl http://127.0.0.1:5984/
# {"couchdb":"Welcome","version":"0.1.0","uuid":"<random>"}
```

### `PUT /{db}`
Create a database. `412` if it already exists. Names follow CouchDB's
rule: a lowercase letter first, then lowercase letters, digits, and
`_ $ ( ) + - /` (max 238 chars); anything else, including `_`-prefixed
system names, is `400 illegal_database_name`.

This is the only route that creates a database. Every other route on a
database that doesn't exist returns
`404 {"error":"not_found","reason":"Database does not exist."}`.

```bash
curl -u user:pass -X PUT http://127.0.0.1:5984/mydb
# {"ok":true}
```

### `DELETE /{db}`
Delete a database, including its attachment files
(`<data>/att/<db>/`). Not part of the replication protocol — added for
test/dev cleanup. `404` if it doesn't exist.

```bash
curl -u user:pass -X DELETE http://127.0.0.1:5984/mydb
```

### `GET /{db}`
Database metadata: `db_name`, `doc_count` (live docs only),
`doc_del_count` (tombstones), `update_seq` and `instance_start_time`
(always `"0"`, as in CouchDB 2+). No `disk_size`/`sizes`.

```bash
curl -u user:pass http://127.0.0.1:5984/mydb
# {"db_name":"mydb","doc_count":0,"doc_del_count":0,"update_seq":0,"instance_start_time":"0"}
```

### `GET /{db}/_all_docs`, `POST /{db}/_all_docs`
Live docs in id order (tombstones left out). Query options:
`include_docs`, `startkey`/`endkey` (JSON strings, inclusive),
`descending`, `skip`, `limit`. Response: `total_rows`, `offset`, `rows`
of `{id, key, value: {rev}}`. `POST` with `{"keys": [...]}` returns one
row per key in request order — `{"key", "error": "not_found"}` for an
unknown id, `value.deleted: true` for a deleted one.

```bash
curl -u user:pass "http://127.0.0.1:5984/mydb/_all_docs?include_docs=true&limit=10"
```

### `/{db}/_design/{name}`
GET/PUT/DELETE design docs (id `_design/{name}`). They are stored and
replicated like any doc; NyxDB doesn't run views.

### `POST /{db}/_compact`
Deletes attachment files that no stored revision refers to any more
(replaced attachments, deleted docs' attachments, and ones only an old,
pruned revision had) — but never one younger than
`NYXDB_ATTACHMENT_GRACE_SECS` (1 hour). Conflict branches' attachments
are kept. Returns `202 {"ok":true}` once done — unlike CouchDB, it runs
before answering. Attachment writes to that database wait while it
runs. The same sweep also runs by itself a minute after startup and
then about daily when the server is idle, so calling this is optional.

```bash
curl -u user:pass -X POST -H "Content-Type: application/json" http://127.0.0.1:5984/mydb/_compact
```

### `GET /{db}/{id}`
Fetch a document. Add `?conflicts=true` to include a `_conflicts` array
when conflicts exist.

```bash
curl -u user:pass http://127.0.0.1:5984/mydb/doc1
# {"_id":"doc1","_rev":"1-<hash>", ...}

curl -u user:pass "http://127.0.0.1:5984/mydb/doc1?conflicts=true"
# {"_id":"doc1","_rev":"3-ddd","_conflicts":["3-ccc"], ...}
```

A deleted document returns `404 {"error":"not_found","reason":"deleted"}`
— different from a document that never existed (`reason:"missing"`).

Revision history (since v0.1.6):

- `?revs=true` adds `_revisions: {"start": N, "ids": [...]}` — the hash
  part of each rev, newest first, walking back from the returned rev.
- `?revs_info=true` adds `_revs_info: [{"rev": ..., "status": ...}]`
  along the winner's history; `status` is `available`, `deleted`, or
  `missing` (an ancestor whose id is known but was never stored).
- `?open_revs=all` or `?open_revs=["1-a","2-b"]` returns a JSON
  **array** instead of one doc: `{"ok": doc}` for each revision found
  (tombstones included, with `_deleted:true`) and `{"missing": rev}`
  for each requested rev that isn't stored. Add `latest=true` to swap
  each requested rev for the leaves descending from it. Honours
  `revs=true` and `attachments=true`. Always JSON — no
  `multipart/mixed`.

```bash
curl -u user:pass "http://127.0.0.1:5984/mydb/doc1?revs=true"
# {"_id":"doc1","_rev":"3-c","_revisions":{"start":3,"ids":["c","b","a"]}, ...}

curl -u user:pass -G "http://127.0.0.1:5984/mydb/doc1" \
  --data-urlencode 'open_revs=["2-b","9-nope"]'
# [{"ok":{"_id":"doc1","_rev":"2-b", ...}},{"missing":"9-nope"}]
```

`?rev=<rev>` returns that specific revision instead of the winner
(tombstones included, with `_deleted:true`); `404 {"reason":"missing"}`
if it isn't stored.

Only leaf revisions have content. Like CouchDB after compaction, an
ancestor keeps its id and parent link (so `_revisions`/`_revs_diff`
work) but not its body, so reading a non-leaf revision — `?rev=`,
`open_revs`, or `_bulk_get` without `latest` — reports it `missing`,
and `revs_info` lists it as `missing`. Each branch keeps its newest
1000 revisions (CouchDB's default `revs_limit`); older ones are dropped.

### `PUT /{db}/{id}`
Two modes, same as `_bulk_docs`, selected by `?new_edits=`:

**Default** (`new_edits` absent or `true`) — the app-write path. Creates
a new revision on top of the body's `_rev` if that's a current leaf (so
an edit of a losing conflict branch extends that branch), otherwise on
top of the current winner. `"_deleted": true` writes a tombstone.
`_id`/`_rev`/`_deleted` are never stored as fields of the doc:

```bash
curl -u user:pass -X PUT http://127.0.0.1:5984/mydb/doc1 \
  -H "Content-Type: application/json" -d '{"foo":"bar"}'
# {"ok":true,"id":"doc1","rev":"1-<hash>"}
```

No optimistic concurrency control here — a `PUT` always succeeds even
if you didn't send the current `_rev`. Use `new_edits=false` if you
need real conflict detection.

### `DELETE /{db}/{id}?rev=<rev>`
Writes a tombstone on top of `rev`, which must be a current leaf
(`409 conflict` otherwise). On a conflicted doc, deleting a *losing*
leaf removes that branch — the request PouchDB's
`db.remove(id, losingRev)` sends to resolve a conflict on a remote.
Without `rev`, the live winner is deleted.

```bash
curl -u user:pass -X DELETE "http://127.0.0.1:5984/mydb/doc1?rev=2-aaaa"
# {"ok":true,"id":"doc1","rev":"3-<hash>"}
```

**`?new_edits=false`** — the single-document form of the replication
push format. You supply the exact `_rev` (and optionally
`_revisions` for full ancestry); the server stores it as-is, creating
a real conflict if it diverges from what's already there — same
semantics as `_bulk_docs` with `new_edits:false`, just for one
document instead of a batch:

```bash
curl -u user:pass -X PUT "http://127.0.0.1:5984/mydb/doc1?new_edits=false" \
  -H "Content-Type: application/json" \
  -d '{"_id":"doc1","_rev":"3-ccc","_revisions":{"start":3,"ids":["ccc","bbb","aaa"]},"foo":"bar"}'
# {"ok":true,"id":"doc1","rev":"3-ccc"}
```

### Attachments

Two ways to attach a file, both supported:

**Inline base64**, embedded in the document body (works with `PUT`,
`_bulk_docs`, and `new_edits:false`):

```bash
curl -u user:pass -X PUT http://127.0.0.1:5984/mydb/doc1 \
  -H "Content-Type: application/json" \
  -d '{"title":"has a file","_attachments":{"note.txt":{"content_type":"text/plain","data":"aGVsbG8="}}}'
```

A plain `GET` on the doc returns the attachment as a **stub**
(`{"content_type":...,"digest":...,"length":...,"stub":true}`), not
the data. Add `?attachments=true` to get the data back inline.

**Standalone upload** — `PUT /{db}/{id}/{attname}` with the raw bytes
as the body (not JSON), `Content-Type` set to the file's real MIME type:

```bash
curl -u user:pass -X PUT http://127.0.0.1:5984/mydb/doc1/note.txt \
  -H "Content-Type: text/plain" --data-binary @note.txt
```

`GET /{db}/{id}/{attname}` fetches the raw bytes back with that
`Content-Type`. Add `?rev=<rev>` to read a specific branch's attachment
— a pulling PouchDB does exactly that for each conflicting branch — or
get `404 missing` if that rev isn't stored. `DELETE /{db}/{id}/{attname}` removes it (the doc's
other fields and other attachments are untouched).

Attachment bytes are files, one per distinct content, named by its
sha256: `<data>/att/<db>/<sha256>` (identical content is stored once,
however many documents or revisions use it). Each is written to a temp
file, fsynced and renamed into place before the write is acknowledged.
Files no revision refers to are deleted by the attachment sweep (see
`POST /{db}/_compact`). Attachments written before v0.1.8 were stored
in sled; the first start of v0.1.8 moves them to files in the
background (`attachment migration finished` in the log), reading them
from sled meanwhile.

Stub digests are CouchDB's `"md5-<base64>"` form — the same digest
PouchDB computes for its own copy, which is how its replicator knows it
already has an attachment and skips downloading it (since v0.1.7;
earlier versions exposed `sha256-` digests, so every pull re-downloaded
every attachment — old stubs still work and are shown as md5).

A stub is a claim that the server already has the bytes. A stub whose
`digest` isn't stored is rejected with `412 {"error":"missing_stub"}`
(per-doc `missing_stub` error inside `_bulk_docs`). On a plain `PUT`, a
bare `{"stub":true}` with no digest keeps the previous revision's
attachment of that name.

Stubs don't include `revpos` (the generation an attachment was added
at) — doesn't affect sync. No `multipart/related` support — only inline
JSON, which is what a PouchDB client sends by default.

### `POST /{db}/_bulk_docs`
Batch write. Two modes:

**Default** (`new_edits` absent or `true`) — same as `PUT`, batched.

```bash
curl -u user:pass -X POST http://127.0.0.1:5984/mydb/_bulk_docs \
  -H "Content-Type: application/json" \
  -d '{"docs":[{"_id":"a","x":1},{"x":2}]}'
# [{"ok":true,"id":"a","rev":"1-<hash>"}, {"ok":true,"id":"<uuid>","rev":"1-<hash>"}]
```

**`new_edits:false`** — the real replication-push format. You supply
the exact `_rev`/`_revisions`; the server stores it as-is, creating a
real conflict if it diverges from what's already there. Returns `[]`
on success; only failures show up in the response.

```bash
curl -u user:pass -X POST http://127.0.0.1:5984/mydb/_bulk_docs \
  -H "Content-Type: application/json" \
  -d '{
    "new_edits": false,
    "docs": [{
      "_id": "doc1",
      "_rev": "3-ccc",
      "_revisions": {"start": 3, "ids": ["ccc", "bbb", "aaa"]},
      "foo": "bar"
    }]
  }'
# []
```

Add `"_deleted": true` to push a tombstone.

### `POST /{db}/_revs_diff`
Given `{docId: [rev, ...]}`, reports which revisions the server doesn't
already have.

```bash
curl -u user:pass -X POST http://127.0.0.1:5984/mydb/_revs_diff \
  -H "Content-Type: application/json" \
  -d '{"doc1": ["1-aaa", "2-bbb", "99-nope"]}'
# {"doc1":{"missing":["99-nope"]}}
```

Not implemented: `possible_ancestors` — a bandwidth optimization for
deep histories, not a correctness issue.

### `POST /{db}/_bulk_get`
Batched fetch. One ok/error entry per `{id, rev?}` request, instead of
failing the whole batch on a miss.

```bash
curl -u user:pass -X POST http://127.0.0.1:5984/mydb/_bulk_get \
  -H "Content-Type: application/json" \
  -d '{"docs":[{"id":"doc1"},{"id":"nope"}]}'
# {"results":[
#   {"id":"doc1","docs":[{"ok":{"_id":"doc1","_rev":"1-<hash>","foo":"bar"}}]},
#   {"id":"nope","docs":[{"error":{"id":"nope","error":"not_found","reason":"missing"}}]}
# ]}
```

Options, in the query string (`?revs=true&latest=true`, what PouchDB
sends) or as top-level body fields (`{"revs":true,"docs":[...]}`):

- `revs=true` — each returned doc carries `_revisions` (`{start, ids}`,
  newest first). **Replication depends on this**: without it a pulling
  PouchDB stores an incoming edit as a new root beside the copy it
  already has, so every edit to an already-synced doc turns into a
  phantom conflict on the other replica (fixed in v0.1.6).
- `latest=true` — a requested `rev` is replaced by the leaf (or
  leaves) descending from it, so one request entry can return several
  docs in its `docs` array.

A revision that exists but is a tombstone (a deleted document, or a
losing conflict branch that was itself a deletion) is still an `ok`
result, with `_deleted:true` in the body — matching real CouchDB.
`_bulk_get` never returns a `"reason":"deleted"` error; only a rev
that doesn't exist at all gets `"reason":"missing"`. This matters for
replication specifically: PouchDB's replicator fetches every open
leaf via `_bulk_get`, including tombstoned ones, and treats *any*
error entry as a hard batch failure — see `doc/changelog.md` for the
real sync failure this caused before it was fixed. (Contrast with a
plain `GET /{db}/{id}` with no `rev`: that one *does* 404 with
`"reason":"deleted"` when the current winner is a tombstone, since
"does this document currently exist" is a genuinely different
question from "give me this specific revision's content.")

### `GET/PUT/DELETE /{db}/_local/{id}`
Replication checkpoints. Last-write-wins, never shows up in `_changes`.
This is how `live:true` resumes after a restart instead of re-syncing
everything. Each write gets the next `0-N` revision, like CouchDB.

```bash
curl -u user:pass -X PUT http://127.0.0.1:5984/mydb/_local/my-checkpoint \
  -H "Content-Type: application/json" -d '{"last_seq":42}'
# {"ok":true,"id":"_local/my-checkpoint","rev":"0-1"}

curl -u user:pass http://127.0.0.1:5984/mydb/_local/my-checkpoint
# {"_id":"_local/my-checkpoint","_rev":"0-1","last_seq":42}
```

### `GET /{db}/_changes`
The "what changed" feed. Query params:

- `since` — sequence number, default `0`. Plain integers, not CouchDB's
  opaque strings — don't mix values between servers. `since=0` means
  "from the very beginning" and is guaranteed to include every
  document ever written, including the first one (fixed in v0.1.5 —
  see `doc/changelog.md` if you're curious about the off-by-one this
  used to have). Values aren't contiguous — expect gaps, and expect a
  jump of roughly a million or more in the numbers themselves after a
  server restart. That's deliberate: `seq` is generated by sled's own
  `generate_id()`, which conservatively advances past its last
  *persisted* checkpoint on reopen so it can never reuse a number after
  an unclean shutdown. Harmless — `since` only needs monotonically
  increasing values to work as an opaque checkpoint, never arithmetic
  on the gaps between them, same as real CouchDB's own opaque tokens.
- `style=all_docs` — include every leaf revision, not just the winner.
- `feed=normal` (default) — one response, everything since `since`.
- `feed=longpoll` — waits for a change (or `timeout` ms, default 60000)
  then responds. This is what PouchDB's `live:true` sync uses.
- `feed=continuous` — streams newline-delimited JSON as changes happen.
- `limit=N` — at most N rows (normal and longpoll); `last_seq` is then
  the last returned row's seq, so `since=<last_seq>` gets the next page.
- `heartbeat=<ms>` (or `true` for 60000) — while waiting, write a
  newline every `heartbeat` ms so the client and any proxy keep the
  connection open. On longpoll, as in CouchDB, a heartbeat overrides
  `timeout`: it waits as long as it takes, and the final body is the
  usual JSON after the newlines. PouchDB sends `heartbeat=10000` and
  gives up on a request after heartbeat + 5 s of silence, so this matters.

```bash
curl -u user:pass "http://127.0.0.1:5984/mydb/_changes"
# {"results":[{"seq":1,"id":"doc1","changes":[{"rev":"1-<hash>"}]}],"last_seq":1}

curl -u user:pass -N "http://127.0.0.1:5984/mydb/_changes?feed=continuous&since=0"
# one JSON line per change, streamed
```

Not implemented: `filter`, `doc_ids`.

### Not implemented at all
`_all_dbs`, `_ensure_full_commit`, `_session`, `_security`, Mango
(`_find`), views, `_replicate`, clustering, compaction,
`multipart/related` attachments. Dropped from scope, not planned —
see `roadmap.md`.

## 5. How we know it works

| Method | Checks | Where |
|---|---|---|
| Unit tests (34) | Winner-picking, conflicts, deletion/recreation, `_revs_diff`, ancestry walk, pruning/stemming, first-write `_changes` visibility, seq-index migration, persisted server id, longpoll race, unreadable docs in `_changes`, attachment compaction, legacy digests | `db/src/*.rs` |
| Real PouchDB client, on 8.0.1 and 9.x | One-shot and `live:true` two-device sync; edits and deletions of already-synced docs stay conflict-free; batches over 100 docs and near the body limit; checkpoint resume across a restart | `test/integration/` (`POUCHDB=9` for 9.x) |
| Revision history | `_bulk_get` `revs`/`latest`; `GET` `revs`/`revs_info`/`open_revs`/`rev`; long histories stay bounded | `test/revisions/` |
| Concurrency | Concurrent writers to one doc keep every branch; `_changes` followers miss nothing under many writers | `test/concurrency/`, `test/changes_follow/` |
| Durability | Every acknowledged write survives a hard kill | `test/durability/` |
| Feeds | longpoll/continuous `heartbeat` and `timeout` | `test/feeds/` |
| HTTP surface | Database creation and names; `_deleted`, `DELETE`, `_local` revs; `_all_docs`, `_design/…`, db info, `_changes?limit` | `test/databases/`, `test/write_semantics/`, `test/read_surface/` |
| Operations | Clean startup failures; bounded memory; no stalls under heavy scans; request timeout, concurrency limit, tracing, CORS max-age | `test/startup/`, `test/memory/`, `test/blocking/`, `test/ops/` |
| Differential vs. real CouchDB | Winner rev, `_conflicts`, `_changes`, `_revs_diff` diffed against a live CouchDB | `test/differential/` |
| Load/soak | Large batch writes + many concurrent `feed=continuous` subscribers | `test/load/` |
| Ported PouchDB test cases | Real edge cases from PouchDB's own test suite | `test/ported/` |
| Attachments | Inline + standalone endpoints, plus a real PouchDB client's `putAttachment`/`getAttachment` | `test/attachments/` |

The differential test matched real CouchDB 3.5.2 exactly on the first
run for a tree with a conflict, a resolution, a deletion, and a
recreation.

The load test found a real bug, since fixed: a `feed=continuous`
subscriber that fell behind the broadcast channel used to silently drop
changes instead of catching up. See `changelog.md`.

## 6. Benchmarks

Full numbers, including memory and disk vs. real CouchDB, are in
[BENCHMARKS.md](BENCHMARKS.md), re-run and updated after every
release (last tested: v0.1.4, 2026-07-27). Summary: ~52x smaller
install, several times faster writes, comparable reads, ~3x lighter
memory, and within ~5% of CouchDB's disk usage per document.

v0.1.7 against v0.1.6 is in BENCHMARKS.md's first section: reads,
`_changes`, bulk writes and memory about the same; single-doc writes
~1.2 ms instead of ~0.2 ms (every write is on disk before it is
acknowledged).

Reproduce with `test/benchmark/`, `test/load/` (scale via
`LOAD_BULK_SIZE`/`LOAD_SUBSCRIBERS`).

## 7. Differences from real CouchDB

1. **Hash algorithm differs** (SHA-256 here, MD5 in CouchDB) — rev ids
   won't match unless dictated explicitly via `new_edits:false`, which
   is what real replication does anyway.
2. **No optimistic concurrency on `PUT /{db}/{id}`** — every write
   succeeds. Use `new_edits:false` for real conflict detection.
3. **`_revs_diff` has no `possible_ancestors`.**
4. **`_changes` sequence tokens are plain integers**, not portable
   between servers.
5. **Attachment stubs don't include `revpos`.** No `multipart/related`
   support — inline JSON only.
6. **No Mango, views, `_security`, `_session`, clustering, or
   server-triggered replication.** Design docs are stored and
   replicated, but nothing runs them.
7. **`POST /{db}/_compact` runs before it answers** and only collects
   attachment bytes (old revision bodies are already dropped on write).
   `GET /{db}` has no `sizes`/`disk_size`.
8. **Plaintext HTTP only** — fine on a trusted LAN, not for anything
   internet-reachable.
9. **Single-node only** — sled is embedded, no clustering.
