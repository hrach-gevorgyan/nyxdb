# NyxDB — CouchDB, rewritten in Rust

NyxDB is a from-scratch reimplementation of CouchDB's HTTP replication
protocol, written in Rust. It implements the subset of that protocol a
real PouchDB client (`db.sync()`, `db.replicate.to/from()`) actually
uses — not a full CouchDB replacement (no Mango queries, no MapReduce
views, no clustering). Point an existing PouchDB app at it instead of a
real CouchDB server and it just works.

## Why Rust?

The original motivation was straightforward: run the same replication
protocol a Node/Erlang CouchDB server implements, but smaller, faster,
and lighter to operate — a single static binary instead of a BEAM VM
and an Erlang/OTP runtime, no JVM-style warm-up, and a fraction of the
install footprint and idle memory. Rust's ownership model also makes
the concurrency this protocol actually needs (many long-lived
`feed=continuous` subscribers, concurrent writers) something the
compiler checks instead of something you hope you got right — see
`doc/AUDIT.md` for a real bug (a lagging `feed=continuous` subscriber
silently dropping changes) that surfaced and got fixed during hardening.
Full reasoning, including two optimization attempts that didn't pan
out, is in [doc/changelog.md](doc/changelog.md).

## Benchmarks

<!-- TODO: fill in current numbers/chart; full methodology and detail
     already live in doc/BENCHMARKS.md — this table is a summary. -->

| | NyxDB | Real CouchDB |
|---|---|---|
| Install size | TBD | TBD |
| Write throughput (docs/sec) | TBD | TBD |
| Read throughput | TBD | TBD |
| Idle memory | TBD | TBD |
| Memory under load | TBD | TBD |
| Disk size (same dataset) | TBD | TBD |
| Startup time | TBD | TBD |
| Replication (`live:true`) latency | TBD | TBD |

<!-- TODO: chart/image -->

Full numbers and methodology: **[doc/BENCHMARKS.md](doc/BENCHMARKS.md)**.

## Compatibility

| Feature | Status |
|---|---|
| `db.sync()` / `db.replicate.to/from()` | ✅ Supported |
| `new_edits:false` replication push, real conflict detection | ✅ Supported |
| `_changes` (normal / longpoll / continuous) | ✅ Supported |
| `_local` checkpoints | ✅ Supported |
| `_bulk_docs`, `_bulk_get`, `_revs_diff` | ✅ Supported |
| Attachments (inline base64 + standalone endpoints) | ✅ Supported |
| HTTP Basic auth | ✅ Supported |
| `multipart/related` attachment uploads | ❌ Not planned |
| Mango (`_find`), MapReduce views | ❌ Not planned |
| `_session` cookie auth | ❌ Not planned |
| Clustering, compaction | ❌ Not planned |

Full endpoint-by-endpoint reference and exact differences from real
CouchDB: **[doc/USAGE.md](doc/USAGE.md)**.

## Install / quickstart

```bash
cargo run --manifest-path db/Cargo.toml
```

Listens on `127.0.0.1:5984` by default, stores data under `./data`.
Override with `NYXDB_ADDR` / `NYXDB_DATA`.

HTTP Basic auth is required on every request except `GET /`. See
[doc/USAGE.md](doc/USAGE.md) for credentials.

```bash
cargo test --manifest-path db/Cargo.toml
```

Already syncing an app against a real CouchDB and want to try NyxDB
instead? See **[doc/MIGRATING.md](doc/MIGRATING.md)**.

## Docs

- **[doc/USAGE.md](doc/USAGE.md)** — how to run it, every endpoint, config, and where it differs from real CouchDB. Start here.
- **[doc/MIGRATING.md](doc/MIGRATING.md)** — point an existing PouchDB app (currently on CouchDB) at this server instead.
- **[doc/BENCHMARKS.md](doc/BENCHMARKS.md)** — speed, size, and memory vs. real CouchDB.
- **[doc/AUDIT.md](doc/AUDIT.md)** — security/stability audit process and findings.
- **[doc/roadmap.md](doc/roadmap.md)** — what's done, what's not.
- **[doc/changelog.md](doc/changelog.md)** — history of what changed and why.

## Layout
- `doc/` — documentation, start with USAGE.md
- `db/` — the Rust server (axum + sled)
- `test/` — integration, load, and differential tests
- `prod/` — Docker deployment files

## Roadmap

See **[doc/roadmap.md](doc/roadmap.md)** for what's done and what's next.

---

NyxDB is an independent, unofficial reimplementation inspired by Apache
CouchDB and is not affiliated with or endorsed by the Apache CouchDB
project.
