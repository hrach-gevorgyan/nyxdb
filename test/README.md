# Tests

- `integration/` — drives a real PouchDB client against a running
  server instance: one-shot replication, `live:true` two-device sync, and
  editing an already-replicated doc without creating phantom conflicts,
  and (`checkpoint_resume.js`, spawns its own server on port 8298)
  resuming from the replication checkpoint across a server restart.
- `databases/` — only `PUT /{db}` creates a database, with a valid
  name; every other route on a missing db is a 404.
- `durability/` — spawns its own server, hard-kills it right after
  acknowledged writes, restarts it; every acknowledged write must
  survive. Needs the release binary (or `NYXDB_BIN`).
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
node test/integration/checkpoint_resume.js   # spawns its own server

# Database creation and naming
node test/databases/run.js

# Hard-kill durability (spawns its own server on port 8299)
node test/durability/run.js

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
