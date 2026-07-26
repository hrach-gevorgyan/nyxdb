# Usage Reference

How to run this server, every endpoint it implements, its config, and
where it differs from real CouchDB. If something here disagrees with
the code, trust the code — check `db/src/routes.rs`.

---

## 1. What this is

A Rust server implementing the part of CouchDB's HTTP replication
protocol that a real PouchDB client (`db.sync()`, `db.replicate.to/from()`)
actually uses. Not a full CouchDB replacement — no Mango queries, no
MapReduce views, no attachments, no clustering.

See [roadmap.md](roadmap.md) for what's done and [changelog.md](changelog.md)
for why decisions were made.

**Status**: Phases 0–3 done (spike, revision trees, live replication,
hardening). Verified with a real PouchDB client, a load test, and a
differential test against real CouchDB 3.5.2 — see §5.

## 2. Running it

```bash
cargo run --manifest-path db/Cargo.toml
```

Binds `127.0.0.1:5984`, stores data under `./data`. Both configurable —
see §3.

On first run it generates HTTP Basic auth credentials and logs the
username (password is in `<data dir>/credentials.json`, or set
`COUCHDB_CLONE_USER`/`COUCHDB_CLONE_PASSWORD` yourself). Every request
needs auth except `GET /`.

```bash
curl http://127.0.0.1:5984/                                   # no auth
curl -u admin:<password> -X PUT http://127.0.0.1:5984/mydb     # needs auth
```

## 3. Configuration

| Variable | Default | Purpose |
|---|---|---|
| `COUCHDB_CLONE_ADDR` | `127.0.0.1:5984` | bind address |
| `COUCHDB_CLONE_DATA` | `./data` | data directory (also holds `credentials.json`) |
| `COUCHDB_CLONE_USER` | generated | admin username |
| `COUCHDB_CLONE_PASSWORD` | generated | admin password (set together with the username above) |
| `COUCHDB_CLONE_CORS_ORIGINS` | unset (CORS off) | comma-separated allowed origins, no wildcard |

Source: `db/src/main.rs`, `db/src/auth.rs`.

## 4. Endpoints

Auth: HTTP Basic on everything except `GET /`. Bodies are JSON.

### `GET /`
Server info. No auth needed.

```bash
curl http://127.0.0.1:5984/
# {"couchdb":"Welcome","version":"0.1.0","uuid":"<random>"}
```

### `PUT /{db}`
Create a database. `412` if it already exists.

```bash
curl -u user:pass -X PUT http://127.0.0.1:5984/mydb
# {"ok":true}
```

### `DELETE /{db}`
Delete a database. Not part of the replication protocol — added for
test/dev cleanup. `404` if it doesn't exist.

```bash
curl -u user:pass -X DELETE http://127.0.0.1:5984/mydb
```

### `GET /{db}`
Database metadata. Only `db_name` and `doc_count` — real CouchDB
returns more (`update_seq`, `disk_size`, etc.), we don't need it.

```bash
curl -u user:pass http://127.0.0.1:5984/mydb
# {"db_name":"mydb","doc_count":0}
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

Not implemented: `?revs=true`, `?open_revs=...`, `?rev=<specific>` (use
`_bulk_get` instead — it supports fetching a specific `rev`).

### `PUT /{db}/{id}`
Write a document. Always creates a new revision on top of the current
winner (last-write-wins). This is the app-write path, not the
replication path — see `_bulk_docs` with `new_edits:false` below.

```bash
curl -u user:pass -X PUT http://127.0.0.1:5984/mydb/doc1 \
  -H "Content-Type: application/json" -d '{"foo":"bar"}'
# {"ok":true,"id":"doc1","rev":"1-<hash>"}
```

No optimistic concurrency control here — a `PUT` always succeeds even
if you didn't send the current `_rev`. Use `new_edits:false` if you
need real conflict detection.

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

### `GET/PUT /{db}/_local/{id}`
Replication checkpoints. Last-write-wins, never shows up in `_changes`.
This is how `live:true` resumes after a restart instead of re-syncing
everything.

```bash
curl -u user:pass -X PUT http://127.0.0.1:5984/mydb/_local/my-checkpoint \
  -H "Content-Type: application/json" -d '{"last_seq":42}'

curl -u user:pass http://127.0.0.1:5984/mydb/_local/my-checkpoint
# {"_id":"_local/my-checkpoint","last_seq":42}
```

### `GET /{db}/_changes`
The "what changed" feed. Query params:

- `since` — sequence number, default `0`. Plain integers, not CouchDB's
  opaque strings — don't mix values between servers.
- `style=all_docs` — include every leaf revision, not just the winner.
- `feed=normal` (default) — one response, everything since `since`.
- `feed=longpoll` — waits for a change (or `timeout` ms) then responds.
- `feed=continuous` — streams newline-delimited JSON as changes happen.
  This is what powers `live:true`.

```bash
curl -u user:pass "http://127.0.0.1:5984/mydb/_changes"
# {"results":[{"seq":1,"id":"doc1","changes":[{"rev":"1-<hash>"}]}],"last_seq":1}

curl -u user:pass -N "http://127.0.0.1:5984/mydb/_changes?feed=continuous&since=0"
# one JSON line per change, streamed
```

Not implemented: `filter`, `doc_ids`, `heartbeat` (matters if you put
this behind a reverse proxy that kills idle connections).

### Not implemented at all
`_all_dbs`, `_ensure_full_commit`, `_session`, `_security`, attachments,
Mango (`_find`), views, `_replicate`, clustering, compaction. Out of
scope unless a real need shows up (see roadmap.md, Phase 4).

## 5. How we know it works

| Method | Checks | Where |
|---|---|---|
| Unit tests (14) | Winner-picking, conflicts, deletion/recreation, `_revs_diff` | `db/src/revtree.rs` |
| Real PouchDB client | One-shot and `live:true` two-device sync | `test/integration/` |
| Differential vs. real CouchDB | Winner rev, `_conflicts`, `_changes`, `_revs_diff` diffed against a live CouchDB | `test/differential/` |
| Load/soak | Large batch writes + many concurrent `feed=continuous` subscribers | `test/load/` |

The differential test matched real CouchDB 3.5.2 exactly on the first
run for a tree with a conflict, a resolution, a deletion, and a
recreation.

The load test found a real bug, since fixed: a `feed=continuous`
subscriber that fell behind the broadcast channel used to silently drop
changes instead of catching up. See `changelog.md`.

## 6. Benchmarks

Full numbers, including memory and disk vs. real CouchDB, are in
[BENCHMARKS.md](BENCHMARKS.md). Summary: ~54x smaller install, ~7.5x
faster writes, comparable reads, 2-3x lighter memory, ~1.3x more disk
per document (the one metric that doesn't win, and not a large one in
absolute terms — see BENCHMARKS.md).

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
5. **No heartbeat on `feed=continuous`.**
6. **No attachments, Mango, views, `_security`, `_session`, clustering,
   or server-triggered replication.**
7. **Plaintext HTTP only** — fine on a trusted LAN, not for anything
   internet-reachable.
8. **Single-node only** — sled is embedded, no clustering.
