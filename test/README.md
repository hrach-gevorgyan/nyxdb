# Testing environment

Per plan §6.

- `unit/` — pure Rust logic tests live alongside the code in `db/src/*.rs`
  (`#[cfg(test)] mod tests`), not here. This folder is for cross-process
  testing that needs a running server and/or external services.
- `integration/` — Node.js harness driving a real PouchDB client
  (`pouchdb-http` adapter) against a locally-run server instance (§6.3).
- `differential/` — harness that runs the same operation sequence against
  this server and a real CouchDB (via `docker-compose.yml` here) and diffs
  the results (§6.2). Manual/pre-release, not CI-blocking by default.

## Running

```bash
# Fast tier: Rust unit tests (from repo root)
cargo test --manifest-path db/Cargo.toml

# Slow tier: differential testing against real CouchDB (needs Docker)
docker compose -f test/differential/docker-compose.yml up -d
node test/differential/run.js
docker compose -f test/differential/docker-compose.yml down

# Integration: real PouchDB client against your server
node test/integration/run.js
```
