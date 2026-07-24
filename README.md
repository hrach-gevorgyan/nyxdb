# couchdb-clone

A minimal, PouchDB-compatible sync server in Rust — implements only the
CouchDB replication protocol surface a real PouchDB client uses.

See [rust-couchdb-clone-plan.md](rust-couchdb-clone-plan.md) for the full
design rationale and [doc/roadmap.md](doc/roadmap.md) for current status
(pre-Phase 0 scaffolding).

## Layout
- `doc/` — changelog, roadmap, open questions, maintenance notes.
- `db/` — main codebase (Rust server, axum + sled).
- `test/` — integration and differential test harnesses.
- `prod/` — Dockerfile and compose file for production deployment.

## Quickstart

```bash
cargo run --manifest-path db/Cargo.toml
```

Runs on `127.0.0.1:5984` by default (override with `COUCHDB_CLONE_ADDR`),
storing data under `./data` (override with `COUCHDB_CLONE_DATA`).

```bash
cargo test --manifest-path db/Cargo.toml
```
