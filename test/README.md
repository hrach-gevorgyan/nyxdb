# Tests

- `integration/` — drives a real PouchDB client against a running
  server instance: one-shot replication, `live:true` two-device sync, and
  editing an already-replicated doc without creating phantom conflicts,
  and (`checkpoint_resume.js`, spawns its own server on port 8298)
  resuming from the replication checkpoint across a server restart.
- `startup/` — startup failures exit with a clear message and code 2/3/4,
  never a panic. Spawns its own server.
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

Rust unit tests live in `db/src/*.rs` (`#[cfg(test)]`), not here.

## Running

```bash
# Unit tests
cargo test --manifest-path db/Cargo.toml

# Integration: real PouchDB client against your server
node test/integration/run.js
node test/integration/live_sync.js
node test/integration/edit_replication.js
node test/integration/delete_replication.js
node test/integration/checkpoint_resume.js   # spawns its own server
node test/integration/large_batches.js       # also spawns a small-limit server

# Startup failures, memory bound (both spawn their own server), stalls
node test/startup/run.js
node test/memory/run.js
node test/blocking/run.js

# Database creation and naming
node test/databases/run.js

# Write semantics
node test/write_semantics/run.js

# Read endpoints
node test/read_surface/run.js

# Hard-kill durability (spawns its own server on port 8299)
node test/durability/run.js

# Request limits, tracing, CORS max-age (spawns its own server on port 8292)
node test/ops/run.js

# _changes followers under concurrent writes
node test/changes_follow/run.js

# longpoll/continuous heartbeat and timeout
node test/feeds/run.js

# Concurrent writers to one doc
node test/concurrency/run.js

# Revision history on read paths
node test/revisions/run.js

# Differential: needs a real CouchDB instance at COUCH_URL
COUCH_USER=admin COUCH_PASSWORD=yourpass node test/differential/run.js

# Load/soak: run against a --release build for meaningful numbers.
# Server needs NYXDB_USER/PASSWORD matching TEST_USER/PASSWORD
# (default: testuser/testpass). Scale with LOAD_BULK_SIZE/LOAD_SUBSCRIBERS.
node test/load/run.js

# Benchmark vs. real CouchDB
COUCH_USER=admin COUCH_PASSWORD=yourpass node test/benchmark/vs_couchdb.js

# Ported PouchDB test cases
node test/ported/pouchdb_tests.js

# Attachments
node test/attachments/run.js
```
