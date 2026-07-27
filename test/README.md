# Tests

- `integration/` — drives a real PouchDB client against a running
  server instance: one-shot replication and `live:true` two-device sync.
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

# Differential: needs a real CouchDB instance at COUCH_URL
COUCH_USER=admin COUCH_PASSWORD=yourpass node test/differential/run.js

# Load/soak: run against a --release build for meaningful numbers.
# Server needs COUCHDB_CLONE_USER/PASSWORD matching TEST_USER/PASSWORD
# (default: testuser/testpass). Scale with LOAD_BULK_SIZE/LOAD_SUBSCRIBERS.
node test/load/run.js

# Benchmark vs. real CouchDB
COUCH_USER=admin COUCH_PASSWORD=yourpass node test/benchmark/vs_couchdb.js

# Ported PouchDB test cases
node test/ported/pouchdb_tests.js

# Attachments
node test/attachments/run.js
```
