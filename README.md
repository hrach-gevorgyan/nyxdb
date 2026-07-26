# couchdb-clone

A minimal, PouchDB-compatible sync server written in Rust. It implements
the subset of CouchDB's replication protocol that a real PouchDB client
uses — not a full CouchDB replacement.

- **[doc/USAGE.md](doc/USAGE.md)** — how to run it, every endpoint, config, and where it differs from real CouchDB. Start here.
- **[doc/BENCHMARKS.md](doc/BENCHMARKS.md)** — speed, size, and memory vs. real CouchDB.
- **[doc/roadmap.md](doc/roadmap.md)** — what's done, what's not.
- **[doc/changelog.md](doc/changelog.md)** — history of what changed and why.

## Layout
- `doc/` — documentation, start with USAGE.md
- `db/` — the Rust server (axum + sled)
- `test/` — integration, load, and differential tests
- `prod/` — Docker deployment files

## Quickstart

```bash
cargo run --manifest-path db/Cargo.toml
```

Listens on `127.0.0.1:5984` by default, stores data under `./data`.
Override with `COUCHDB_CLONE_ADDR` / `COUCHDB_CLONE_DATA`.

HTTP Basic auth is required on every request except `GET /`. See
[doc/USAGE.md](doc/USAGE.md) for credentials.

```bash
cargo test --manifest-path db/Cargo.toml
```
