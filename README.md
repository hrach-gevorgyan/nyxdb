<p align="center">
  <a href="LICENSE-MIT"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-8B5CF6?style=flat-square"></a>
  <a href="https://github.com/hrach-gevorgyan/nyxdb/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/hrach-gevorgyan/nyxdb?style=flat-square&color=8B5CF6"></a>
  <a href="https://github.com/hrach-gevorgyan/nyxdb/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/hrach-gevorgyan/nyxdb/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/hrach-gevorgyan/nyxdb/commits/master"><img alt="Last commit" src="https://img.shields.io/github/last-commit/hrach-gevorgyan/nyxdb?style=flat-square&color=8B5CF6"></a>
  <img alt="Commit activity" src="https://img.shields.io/github/commit-activity/m/hrach-gevorgyan/nyxdb?style=flat-square&color=8B5CF6">
</p>

<p align="center">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-000000?style=flat-square&logo=rust&logoColor=white">
  <img alt="axum" src="https://img.shields.io/badge/axum-000000?style=flat-square">
  <img alt="sled" src="https://img.shields.io/badge/sled-embedded%20KV-000000?style=flat-square">
  <img alt="CouchDB protocol" src="https://img.shields.io/badge/protocol-CouchDB%20replication-E2041B?style=flat-square&logo=apachecouchdb&logoColor=white">
  <img alt="PouchDB compatible" src="https://img.shields.io/badge/PouchDB-compatible-E2041B?style=flat-square&logo=apachecouchdb&logoColor=white">
</p>

# NyxDB

**CouchDB's replication protocol, rewritten in Rust.**
*Smaller. Faster. Drop-in.*

A from-scratch reimplementation of the exact HTTP replication protocol
a real PouchDB client speaks — `db.sync()`, `db.replicate.to/from()`,
real conflict detection, live `_changes`, attachments. Point an
existing PouchDB app at it instead of a real CouchDB server, and it
just works. Not a full CouchDB replacement — no Mango, no MapReduce
views, no clustering — and not trying to be one.

Built specifically to be the sync backend for local-first apps that
already speak this protocol, not a general-purpose CouchDB competitor.
(Currently powers [Offlog](https://github.com/hrach-gevorgyan/offlog)'s
device-to-device sync.)

**Jump to:** [Why Rust](#why-rust) · [Benchmarks](#benchmarks) ·
[Compatibility](#compatibility) · [Install / quickstart](#install--quickstart) ·
[Documentation](#documentation) · [Contributing](#contributing)

---

## Why Rust?

The original motivation was straightforward: run the same replication
protocol a Node/Erlang CouchDB server implements, but smaller, faster,
and lighter to operate — a single static binary instead of a BEAM VM
and an Erlang/OTP runtime, no JVM-style warm-up, and a fraction of the
install footprint and idle memory. Rust's ownership model also makes
the concurrency this protocol actually needs (many long-lived
`feed=continuous` subscribers, concurrent writers) something the
compiler checks instead of something you hope you got right — see
[doc/AUDIT.md](doc/AUDIT.md) for a real bug (a lagging `feed=continuous`
subscriber silently dropping changes) that surfaced and got fixed
during hardening. Full reasoning, including optimization attempts that
didn't pan out, is in [doc/changelog.md](doc/changelog.md).

**Built like a real product, not a prototype.** Every release: full
test suite (unit, integration with a real PouchDB client, ported
PouchDB conflict-resolution cases, load/soak, and a differential test
against a live real CouchDB instance), a security audit process
([doc/AUDIT.md](doc/AUDIT.md)), and benchmarks re-run and re-verified
— not left stale from an earlier version. See
[doc/changelog.md](doc/changelog.md) for the honest history, including
real bugs found live-testing against a real app and fixed, not swept
under the rug.

## Benchmarks

One machine, one run each, release build vs. a real local CouchDB
3.5.2 — order-of-magnitude numbers, not a lab-grade benchmark. Full
methodology, historical progression, and honest caveats in
**[doc/BENCHMARKS.md](doc/BENCHMARKS.md)**, re-run and updated after
every release.

**Last tested: v0.1.5, 2026-07-27 17:56 UTC.**

| | NyxDB | Real CouchDB | Ratio |
|---|---|---|---|
| Install size | 4.39 MB | 229 MB | **~52x smaller** |
| Write throughput (5,000 docs, one `_bulk_docs`) | ~16,340 docs/sec | ~3,870 docs/sec | **~4.2x faster** |
| Read throughput (200 sequential `GET`) | 10.66ms/req | 10.79ms/req | ~1.01x faster |
| Idle memory | 27.8 MB | ~93.5 MB | **~3.4x lighter** |
| Memory after a 5,000-doc write | 35.4 MB | ~116 MB | **~3.3x lighter** |
| Disk size (same 5,000-doc dataset) | 1.84 MB | 1.74 MB | ~1.06x more |
| Startup time | <20ms | seconds (Erlang/OTP boot) | — |
| `_changes` in-process notification latency | ~0.14ms | not measured | — |

Write throughput varies noticeably run to run on a shared dev machine
(this round: 4.2x; earlier releases measured 4.75x and 6.5x at
different points in the same long session) — see
[doc/BENCHMARKS.md](doc/BENCHMARKS.md) for the honest caveat. Disk
size is the one metric that doesn't outright win, and it's close —
see that file for what the gap actually costs at real-world scale
(it's small).

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

## Documentation

| Document | What's in it |
|---|---|
| [doc/USAGE.md](doc/USAGE.md) | How to run it, every endpoint, config, and where it differs from real CouchDB — start here |
| [doc/MIGRATING.md](doc/MIGRATING.md) | Point an existing PouchDB app (currently on CouchDB) at this server instead |
| [doc/BENCHMARKS.md](doc/BENCHMARKS.md) | Speed, size, and memory vs. real CouchDB, re-run every release |
| [doc/AUDIT.md](doc/AUDIT.md) | Security/stability audit process and findings |
| [SECURITY.md](SECURITY.md) | How to report a vulnerability |
| [doc/roadmap.md](doc/roadmap.md) | What's done, what's not |
| [doc/changelog.md](doc/changelog.md) | History of what changed and why, including real bugs found and fixed |

## Layout

```
db/     the Rust server (axum + sled)
doc/    documentation — start with doc/USAGE.md
test/   integration, load, ported PouchDB, and differential tests
prod/   Docker deployment files
```

## Contributing

Built solo, with Claude doing the hands-on-keyboard engineering under
direct review — the codebase and `doc/` are written so an AI assistant
(or a human) can pick it up the same way for a fork. Real code review,
fixes, and edge cases found through actual use are genuinely wanted.
See [doc/roadmap.md](doc/roadmap.md) for what's open, and
[SECURITY.md](SECURITY.md) if what you found is a vulnerability rather
than a bug.

## License

Dual-licensed under either [MIT](LICENSE-MIT) or [Apache License, Version 2.0](LICENSE-APACHE), at your option.

---

<p align="center"><sub>
NyxDB is an independent, unofficial reimplementation inspired by Apache CouchDB and is not affiliated with or endorsed by the Apache CouchDB project.
</sub></p>
