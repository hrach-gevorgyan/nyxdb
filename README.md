# couchdb-clone

A minimal, PouchDB-compatible sync server in Rust — implements only the
CouchDB replication protocol surface a real PouchDB client uses.

**For how to run it, every endpoint with examples, config, benchmarks,
and exactly where this differs from real CouchDB, see
[doc/USAGE.md](doc/USAGE.md).** That's the reference doc — this README
is just an index. For a direct speed/size comparison against real
CouchDB, see [doc/BENCHMARKS.md](doc/BENCHMARKS.md).

See [rust-couchdb-clone-plan.md](rust-couchdb-clone-plan.md) for the full
design rationale and [doc/roadmap.md](doc/roadmap.md) for current status
(Phases 0–3 complete).

## Layout
- `doc/` — [USAGE.md](doc/USAGE.md) (start here), changelog, roadmap,
  open questions, maintenance notes.
- `db/` — main codebase (Rust server, axum + sled).
- `test/` — integration, load, and differential test harnesses.
- `prod/` — Dockerfile and compose file for production deployment.

## Quickstart

```bash
cargo run --manifest-path db/Cargo.toml
```

Runs on `127.0.0.1:5984` by default (override with `COUCHDB_CLONE_ADDR`),
storing data under `./data` (override with `COUCHDB_CLONE_DATA`). HTTP
Basic auth is required on every request except `GET /` — see
[doc/USAGE.md](doc/USAGE.md) for credentials setup and the full endpoint
reference.

```bash
cargo test --manifest-path db/Cargo.toml
```
