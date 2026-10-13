# Tests

- `integration/` — drives a real PouchDB client against a running
  server instance: one-shot replication, `live:true` two-device sync,
  editing an already-replicated doc without creating phantom conflicts,
  deletions reaching replicas that already have the doc, batches over 100
  docs and near the body limit (`large_batches.js`), and
  (`checkpoint_resume.js`, spawns its own server on port 8298) resuming
  from the replication checkpoint across a server restart. Runs on
  PouchDB 8.0.1 by default; `POUCHDB=9` runs it on 9.x (what Offlog
  ships). Both are installed by `npm install --prefix test/integration`.
- `startup/` — startup failures exit with a clear message and code 2/3/4,
  never a panic. Spawns its own server.
- `att_disk/` — own server: replacing attachments, deleting docs and
  deleting the database free their disk space; uploads during
  back-to-back sweeps all survive.
- `att_memory/` — own server: 1 GB of attachments written and read back,
  memory stays flat.
- `migration/` — the real v0.1.7 binary (`NYXDB_OLD_BIN`; `run_all.js`
  builds it from the tag) writes attachments; this build is killed twice
  mid-migration; nothing is lost.
- `memory/` — memory stays bounded while ~100 MB of small docs are
  written and read back. Spawns its own server.
- `blocking/` — `GET /` stays fast while many full `_changes` scans run.
- `databases/` — only `PUT /{db}` creates a database, with a valid
  name; every other route on a missing db is a 404.
- `write_semantics/` — `_deleted`, `DELETE` (incl. PouchDB resolving a
  conflict on a remote), writes onto a losing leaf, `_local` revisions.
  Uses `test/integration`'s PouchDB install.
- `read_surface/` — `_all_docs` (GET and POST `keys`), `_design/...` ids,
  `GET /{db}` metadata, `_changes?limit=`.
- `durability/` — spawns its own server, hard-kills it right after
  acknowledged writes, restarts it; every acknowledged write must
  survive. Needs the release binary (or `NYXDB_BIN`).
- `perf/` — throughput guards with a wide margin: a 2,000-doc
  `_bulk_docs` (app writes and replication push) must take under 400 ms.
- `connections/` — own server (port 8286) with short connection limits:
  header and idle timeouts, the connection cap, and live feeds outliving
  both timeouts.
- `ops/` — spawns its own server (port 8292) with tiny limits: request
  timeout, concurrency limit, `_changes` exempt from both, request
  tracing, CORS preflight `max-age`. Needs the release binary.
- `changes_follow/` — polling and continuous `_changes` followers
  while many writers run; neither may miss a change.
- `feeds/` — `_changes` longpoll/continuous `heartbeat` and `timeout`
  on the wire.
- `concurrency/` — many simultaneous writes to the same doc; every
  acknowledged revision must survive.
- `revisions/` — revision history on the read paths: `_bulk_get` with
  `revs`/`latest`, `GET` with `revs`/`revs_info`/`open_revs`.
- `differential/` — runs the same operations against this server and a
  real CouchDB, diffs the results. Needs a real CouchDB instance
  (Docker, native install, or hosted — any works).
- `load/` — large `_bulk_docs` batch plus many concurrent
  `feed=continuous` subscribers. Caught a real bug once (see
  `doc/changelog.md`).
- `benchmark/` — speed/size/memory comparison against real CouchDB.
- `ported/` — test cases ported from PouchDB's own test suite
  (conflict resolution, idempotent replay, edge-case doc ids).
- `attachments/` — inline + standalone attachment endpoints, plus a
  real PouchDB client's `putAttachment`/`getAttachment`.

Rust unit tests live in `db/src/**/*.rs` (`#[cfg(test)]`), not here.
Shared helpers (config, `check`/`finish`, `spawnServer`) are in
`lib/harness.js`.

## Running

Everything, the way CI does it (release build with zero warnings,
clippy, unit tests, every suite against a scratch server on port 8281,
then the PouchDB-driven suites again on PouchDB 9.x):

```bash
npm install --prefix test/integration   # once
node test/run_all.js                    # exit 0 = all passed
node test/run_all.js --no-rust feeds    # skip cargo; only suites matching "feeds"
```

One suite against a server you started yourself:

```bash
SERVER_URL=http://127.0.0.1:5984 OUR_USER=admin OUR_PASSWORD=secret node test/feeds/run.js
POUCHDB=9 node test/integration/live_sync.js   # PouchDB-driven suites: pick 8 (default) or 9
```

`OUR_USER`/`OUR_PASSWORD` and `TEST_USER`/`TEST_PASSWORD` are the same
setting (either works; default `testuser`/`testpass`).

Suites that spawn their own server need the release binary (or
`NYXDB_BIN`) and use these ports: startup 8297, memory 8295, att_memory 8289, att_disk 8291,
migration 8287, durability
8299, ops 8292, checkpoint_resume 8298, large_batches 8294.

Need a real CouchDB (not run by `run_all.js` or CI):

```bash
COUCH_USER=admin COUCH_PASSWORD=secret node test/differential/run.js
COUCH_USER=admin COUCH_PASSWORD=secret node test/benchmark/vs_couchdb.js
```
