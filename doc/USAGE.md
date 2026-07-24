# Usage Reference

This is the single reference for how to run this server, every endpoint
it implements, its configuration, measured performance, and — most
importantly — **exactly where it differs from real CouchDB**. Written to
be equally usable by a person and by an AI agent picking up this repo
cold: every claim here is either a fact about the code (with a file
reference) or a number from an actual test run in this repo, not a
projection.

If something here conflicts with the code, the code wins — this file
covers the state as of the commit that added it; re-verify against
`db/src/routes.rs` if it's been a while.

---

## 1. What this is

A Rust server implementing the subset of CouchDB's HTTP replication
protocol that a real PouchDB client (`db.sync()`, `db.replicate.to/from()`)
actually uses. It is **not** a general CouchDB replacement — no Mango
queries, no MapReduce views, no attachments, no clustering. See
[rust-couchdb-clone-plan.md](../rust-couchdb-clone-plan.md) for the full
design rationale and [roadmap.md](roadmap.md) for what's implemented.

**Status**: Phases 0–3 complete (spike, real revision trees, live
replication, hardening). Verified three independent ways: a real PouchDB
client (one-shot and `live:true` two-device sync), a load test, and a
differential test against real CouchDB 3.5.2 (see §5).

## 2. Running it

```bash
cargo run --manifest-path db/Cargo.toml
```

Binds `127.0.0.1:5984` by default, storing data under `./data`. Both are
configurable — see §3.

On first run it generates HTTP Basic auth credentials and logs the
username (not the password — read `<data dir>/credentials.json`, or set
`COUCHDB_CLONE_USER`/`COUCHDB_CLONE_PASSWORD` explicitly). **Every
request needs these credentials except `GET /`.**

```bash
# quick smoke test
curl http://127.0.0.1:5984/                                   # no auth needed
curl -u admin:<password> -X PUT http://127.0.0.1:5984/mydb     # auth needed
```

## 3. Configuration (environment variables)

| Variable | Default | Purpose |
|---|---|---|
| `COUCHDB_CLONE_ADDR` | `127.0.0.1:5984` | bind address |
| `COUCHDB_CLONE_DATA` | `./data` | sled data directory (also where `credentials.json` lives) |
| `COUCHDB_CLONE_USER` | *(generated)* | pins the admin username instead of using the generated/stored one |
| `COUCHDB_CLONE_PASSWORD` | *(generated)* | pins the admin password; **must be set together with** `COUCHDB_CLONE_USER` |
| `COUCHDB_CLONE_CORS_ORIGINS` | *(unset = CORS disabled)* | comma-separated exact-origin allowlist (e.g. `http://localhost:3000,capacitor://localhost`); no wildcard support |

Source of truth: `db/src/main.rs`, `db/src/auth.rs`.

## 4. Endpoint reference

All paths below are relative to the server root. `{db}` and `{id}` are
literal path segments you substitute. **Auth**: HTTP Basic, required on
everything except `GET /`. All request/response bodies are JSON.

### `GET /`
Server identification. No auth required (matches real CouchDB — clients
probe this for feature detection before necessarily having db-specific
credentials).

```bash
curl http://127.0.0.1:5984/
# {"couchdb":"Welcome","version":"0.1.0","uuid":"<random>"}
```

### `PUT /{db}`
Create a database. Returns **`412 Precondition Failed`** if it already
exists, matching CouchDB.

```bash
curl -u user:pass -X PUT http://127.0.0.1:5984/mydb
# {"ok":true}
```

### `GET /{db}`
Database metadata.

```bash
curl -u user:pass http://127.0.0.1:5984/mydb
# {"db_name":"mydb","doc_count":0}
```
*Difference from real CouchDB*: real CouchDB's `_all_docs`-style metadata
includes `update_seq`, `disk_size`, etc. We only return `db_name` and
`doc_count` — nothing that reads this beyond a sanity check needs more.

### `GET /{db}/{id}`
Fetch a document (current winning revision). Query params:
- `conflicts=true` — adds a `_conflicts` array (list of losing, non-deleted
  leaf revs) if any exist; omitted entirely if empty.

```bash
curl -u user:pass http://127.0.0.1:5984/mydb/doc1
# {"_id":"doc1","_rev":"1-<hash>","...fields...}

curl -u user:pass "http://127.0.0.1:5984/mydb/doc1?conflicts=true"
# {"_id":"doc1","_rev":"3-ddd","_conflicts":["3-ccc"],"...fields...}
```
A fully-deleted document (winning revision is a tombstone) returns
**`404 {"error":"not_found","reason":"deleted"}`** — distinct from a
document that never existed (`reason:"missing"`), matching CouchDB.

*Not implemented*: `?revs=true` (revision history), `?open_revs=...`
(fetch specific/all leaf revisions in one call), `?rev=<specific>`
(fetch a non-winning revision by id — use `_bulk_get` instead, which
does support requesting a specific `rev`).

### `PUT /{db}/{id}`
Write a document. **Always mints a new revision** based on the current
winner (single-writer, last-write-wins) — this is the app-level write
path, not the replication-push path (see `_bulk_docs` with
`new_edits:false` below for that).

```bash
curl -u user:pass -X PUT http://127.0.0.1:5984/mydb/doc1 \
  -H "Content-Type: application/json" -d '{"foo":"bar"}'
# {"ok":true,"id":"doc1","rev":"1-<hash>"}
```
*Not implemented*: conditional writes via `If-Match`; you can't reject a
write based on a client-supplied `_rev` not matching the current
winner — every `PUT` here always succeeds and creates a new generation
on top of whatever the current winner is. **This means concurrent-write
conflict detection via `_rev` mismatch (CouchDB's normal optimistic
concurrency control) is not enforced on this path.** Use
`new_edits:false` (below) if you need exact revision control.

### `POST /{db}/_bulk_docs`
Batch write. Two distinct modes selected by the `new_edits` field:

**Default (`new_edits` absent or `true`)** — same semantics as `PUT
/{db}/{id}`, batched: server mints a new rev per doc from its current
winner.

```bash
curl -u user:pass -X POST http://127.0.0.1:5984/mydb/_bulk_docs \
  -H "Content-Type: application/json" \
  -d '{"docs":[{"_id":"a","x":1},{"x":2}]}'
# [{"ok":true,"id":"a","rev":"1-<hash>"},{"ok":true,"id":"<generated-uuid>","rev":"1-<hash>"}]
```

**`new_edits:false`** — the actual replication-push wire format: you
supply the exact `_rev` and `_revisions` (ancestry) for each doc; the
server stores it verbatim, creating a **real conflict** if it diverges
from an existing branch, and is idempotent against replay. Returns `[]`
on full success (matching CouchDB — the rev was yours, not newly minted,
so there's nothing new to report); only per-doc failures appear in the
array.

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
Set `"_deleted": true` on a doc to push a tombstone.

Source: `db/src/routes.rs::bulk_docs`, `bulk_docs_new_edits_true`,
`bulk_docs_new_edits_false`.

### `POST /{db}/_revs_diff`
Given `{docId: [rev, ...]}`, reports which of the requested revisions
this server does **not** already have.

```bash
curl -u user:pass -X POST http://127.0.0.1:5984/mydb/_revs_diff \
  -H "Content-Type: application/json" \
  -d '{"doc1": ["1-aaa", "2-bbb", "99-nope"]}'
# {"doc1":{"missing":["99-nope"]}}
```
*Not implemented*: `possible_ancestors` in the response — real CouchDB
sometimes suggests ancestor revisions to help a client send a more
compact diff. We only report presence/absence. This is a known,
documented gap (`roadmap.md`), not a bug — it doesn't break correctness,
only optimizes bandwidth on deep histories.

### `POST /{db}/_bulk_get`
Batched doc fetch: one ok/error entry per `{id, rev?}` request, instead
of failing the whole batch on a single miss.

```bash
curl -u user:pass -X POST http://127.0.0.1:5984/mydb/_bulk_get \
  -H "Content-Type: application/json" \
  -d '{"docs":[{"id":"doc1"},{"id":"nope"}]}'
# {"results":[
#   {"id":"doc1","docs":[{"ok":{"_id":"doc1","_rev":"1-<hash>","foo":"bar"}}]},
#   {"id":"nope","docs":[{"error":{"id":"nope","error":"not_found","reason":"missing"}}]}
# ]}
```

### `GET /{db}/_local/{id}` / `PUT /{db}/_local/{id}`
Replication checkpoints. Single-revision, last-write-wins, never appear
in `_changes` — this is how `live:true` resumes after a restart instead
of re-syncing everything.

```bash
curl -u user:pass -X PUT http://127.0.0.1:5984/mydb/_local/my-checkpoint \
  -H "Content-Type: application/json" -d '{"last_seq":42}'
# {"ok":true,"id":"_local/my-checkpoint","rev":"0-1"}

curl -u user:pass http://127.0.0.1:5984/mydb/_local/my-checkpoint
# {"_id":"_local/my-checkpoint","last_seq":42}
```

### `GET /{db}/_changes`
The core "what changed" feed. Query params:
- `since` — integer sequence number, default `0`. **Note**: unlike real
  CouchDB's opaque string sequence tokens, ours are plain integers —
  fine for this server talking to itself, but don't assume the token
  format is portable to/from a real CouchDB's `since` value.
- `style=all_docs` — include every leaf revision per doc (needed so
  conflicts replicate too), not just the winner.
- `feed=normal` (default) — one JSON response, everything since `since`.
- `feed=longpoll` — holds the connection until something changes or
  `timeout` (ms, default 60000) elapses, then responds like normal mode.
- `feed=continuous` — streams newline-delimited JSON: catch-up rows from
  storage first, then live rows as writes happen. This is what powers
  `live:true`.

```bash
curl -u user:pass "http://127.0.0.1:5984/mydb/_changes"
# {"results":[{"seq":1,"id":"doc1","changes":[{"rev":"1-<hash>"}]}],"last_seq":1}

curl -u user:pass -N "http://127.0.0.1:5984/mydb/_changes?feed=continuous&since=0"
# {"seq":1,"id":"doc1","changes":[{"rev":"1-<hash>"}]}
# ...one line per change, streamed as they happen...
```
One row per doc even if it changed multiple times in range (dedup keeps
the highest seq) — matches CouchDB's `_changes` semantics.

*Not implemented*: `filter` functions, `doc_ids` filtering, `heartbeat`
(a periodic newline to keep idle continuous connections alive through
proxies/load balancers that time out otherwise) — not yet needed for
direct client↔server connections, but relevant if you ever put this
behind a reverse proxy.

### Everything NOT implemented at all
`_all_dbs`, `_ensure_full_commit` (no-op elsewhere in real deployments;
we don't have the endpoint at all — harmless since our storage is
durable per-write anyway, but a client that calls it will get a plain
404, not a success), `_session` (cookie auth), `_security`, attachments
(`_attachments`, multipart MIME), Mango (`_find`), MapReduce views
(`_design`/`_view`), `_replicate` (server-triggered replication —
client-driven only), clustering/sharding, compaction endpoints. All are
explicitly out of scope per the plan (§3) unless a concrete need shows
up (Phase 4).

## 5. How we know this actually works

Not just "should work" — three different verification methods, all in
this repo and re-runnable:

| Method | What it checks | Location |
|---|---|---|
| Unit tests | Revision-tree winner-picking, conflicts, deletion/recreation, `_revs_diff` logic — 14 tests | `db/src/revtree.rs` (`cargo test --manifest-path db/Cargo.toml`) |
| Real PouchDB client | One-shot `db.replicate.to()` and two-device `db.sync({live:true, retry:true})` convergence | `test/integration/run.js`, `test/integration/live_sync.js` |
| Differential vs real CouchDB | Winner rev, `_conflicts`, `_changes` content, `_revs_diff` — identical operation sequence run against both servers and diffed | `test/differential/run.js` (needs a real CouchDB instance; see `test/README.md`) |
| Load/soak | Large `_bulk_docs` batch + many concurrent `feed=continuous` subscribers, verifying nobody misses a change | `test/load/run.js` |

**The differential test found something reassuring, not alarming**:
against a real local CouchDB 3.5.2, winner-picking, `_conflicts`,
`_changes` content, and `_revs_diff` all matched **exactly** on the
first run, for a tree exercising a conflict, a deeper-generation
resolution, a deletion, and a recreation.

**The load test found a real bug** (now fixed): a `feed=continuous`
subscriber that fell behind the internal broadcast channel's capacity
under heavy write load used to silently drop the changes it missed
instead of catching up. See `doc/changelog.md` for the fix
(`db/src/routes.rs::changes_continuous`, now a stateful stream that
re-queries storage on lag instead of trusting the broadcast channel
alone).

## 6. Benchmark numbers

Measured on the development machine used to build this (Windows,
`cargo run --release`), from `test/load/run.js`. **These are illustrative,
not a formal benchmark suite** — no controlled hardware, no repeated
trials, no comparison against real CouchDB's own throughput. Treat them
as "order of magnitude, and confirmation nothing falls over," not as a
performance guarantee.

| Scenario | Result |
|---|---|
| 2,000-doc `_bulk_docs` batch, 20 concurrent `feed=continuous` subscribers | 236ms to write (~8,475 docs/sec); all 20 subscribers received all 2,000 changes; server memory 30MB→46MB (fresh process) |
| 8,000-doc batch, 50 subscribers | 1.4s to write (~5,682 docs/sec); all received; memory 29MB→51MB (fresh process) |
| 15,000-doc batch, 80 subscribers | 3.0s to write (~5,059 docs/sec); all received; memory 51MB→69MB (same process as the row above, run back-to-back) |

Throughput-per-doc drops somewhat as batch size/subscriber count grows
(more fan-out work per write), but stayed well above what any real
personal/small-team deployment would need, and memory growth was modest
and did not compound across repeated runs in the same test session.

Re-run with different scale via `LOAD_BULK_SIZE`/`LOAD_SUBSCRIBERS` env
vars — see `test/README.md`.

**For a direct comparison against real CouchDB** — install size, write/
read throughput, and on-disk size for identical data (including one
honest downside: this server currently uses *more* disk per document
than CouchDB, not less) — see [BENCHMARKS.md](BENCHMARKS.md).

## 7. Where this differs from real CouchDB (summary)

A consolidated version of the "not implemented"/"difference" notes
scattered through §4, for quick scanning:

1. **Revision hash algorithm differs** — SHA-256-truncated here, MD5 in
   real CouchDB. Rev ids will never match byte-for-byte between the two
   servers *unless* you dictate them explicitly via `new_edits:false`
   (which is what real replication does anyway, so this doesn't affect
   actual client↔server sync — only matters if you're diffing two
   independently-written docs' auto-generated revs).
2. **No optimistic concurrency control on `PUT /{db}/{id}`** — every
   `PUT` succeeds and creates a new generation on the current winner;
   there's no `_rev` mismatch rejection. `new_edits:false` via
   `_bulk_docs` is the only path with real conflict semantics.
3. **`_revs_diff` has no `possible_ancestors`** — presence/absence only.
4. **`_changes` sequence tokens are plain integers**, not CouchDB's
   opaque strings — fine internally, not portable as a value between a
   real CouchDB and this server.
5. **No heartbeat on `feed=continuous`** — matters if a reverse proxy or
   load balancer sits between client and server with its own idle
   timeout.
6. **No attachments, Mango/`_find`, views, `_security`, `_session`,
   clustering, or server-triggered replication** — all explicitly out of
   scope unless Phase 4 happens.
7. **Plaintext HTTP only** — no built-in TLS. Fine on a trusted LAN, not
   for anything internet-reachable (see `doc/open-questions.md`).
8. **Single-node only** — sled is an embedded, single-process store; no
   clustering or sharding, unlike CouchDB 4.x's FoundationDB backing.
