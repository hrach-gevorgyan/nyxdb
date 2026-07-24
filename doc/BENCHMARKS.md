# Benchmarks vs Real CouchDB

Measured on one machine (Windows, this development box), one run each,
against a real local CouchDB 3.5.2 install and this server's release
build. **These are single-run numbers on one machine, not a rigorous
statistical benchmark** — no warm-up runs, no repeated trials, no varied
hardware. Good for "what's the actual order of magnitude," not for a
performance guarantee. Reproduce with `test/benchmark/vs_couchdb.js` and
your own CouchDB instance — see that file for exact methodology.

Every number below is from an actual run in this repo, not a projection.

---

## Install / binary footprint

| | This server | Real CouchDB 3.5.2 |
|---|---|---|
| **What you ship/install** | One ~4.42 MB executable (`couchdb-clone.exe`, zstd statically linked in) | Full install: Erlang/OTP runtime + ICU + a bundled Lucene-based search engine (`nouveau`) + CouchDB itself |
| **Total size** | 4.42 MB | 229 MB |
| **Ratio** | — | **~52x larger** |

CouchDB's install breakdown (`test/benchmark` methodology, measured via
`du`/`Get-ChildItem`):

| Component | Size |
|---|---|
| `lib` (Erlang libraries) | 52 MB |
| `bin` | 49 MB |
| `erts-14.2.5.15` (Erlang runtime system) | 47 MB |
| `nouveau` (bundled Lucene/Java search engine) | 39 MB |
| `share` | 28 MB |
| `var` (actual database data — excluded from the "software" comparison above) | 12 MB |
| `data` | 2.9 MB |
| everything else | ~0.5 MB |

This is the number that motivated the whole project (see
[rust-couchdb-clone-plan.md](../rust-couchdb-clone-plan.md) §1): a
~230 MB Erlang runtime is a lot to vendor into an app just for optional
sync, when a real client only ever calls ~10 HTTP endpoints.

**Caveat**: this compares "what you have to install" — it's not
apples-to-apples on *capability*. CouchDB's 229 MB buys you full
MapReduce views, Mango queries, clustering, attachments, and a bundled
full-text search engine, none of which this server implements (see
[USAGE.md §7](USAGE.md#7-where-this-differs-from-real-couchdb-summary)).
The comparison is fair only for "the replication-protocol surface a
PouchDB client actually uses," which is this project's entire premise.

## Write throughput

5,000 realistic small JSON documents (~200 bytes each — task-manager-shaped
records, not toy `{"a":1}` payloads), written via a single `_bulk_docs`
request to each server:

| | Time | Throughput |
|---|---|---|
| This server | 167ms | ~29,940 docs/sec |
| Real CouchDB | 1,330ms | ~3,759 docs/sec |
| **Ratio** | | **~7.96x faster** |

Slower than an earlier measurement of this server (88ms/~56,800 docs/sec)
— the difference is the zstd compression enabled below, which trades
some write-side CPU for a large disk-size win. Still comfortably faster
than CouchDB. This gap is expected, not a fluke either way — CouchDB's
write path does more per document by design (its own MVCC B-tree
updates, view group bookkeeping hooks, replication-relevant metadata)
that this server's much smaller scope skips entirely. It is not evidence
that sled is inherently faster than CouchDB's storage engine in general.

## Read throughput

200 sequential single-document `GET` requests (i.e. round-trip latency,
not raw server-side processing time — dominated by HTTP/TCP overhead on
localhost either way):

| | Time | Avg latency |
|---|---|---|
| This server | 1,111ms | 5.55ms/req |
| Real CouchDB | 1,238ms | 6.19ms/req |
| **Ratio** | | **~1.11x faster** |

Much closer than the write benchmark — reads are dominated by
per-request HTTP overhead on both servers, not by storage-engine
differences, so this is a much weaker signal than the write number
above.

## On-disk size — found a real gap, then closed most of it

Same 5,000 documents, disk space actually used:

| | Size | Per doc |
|---|---|---|
| **Before fix**: this server (sled, no compression, hand-rolled seq counter) | 6.1 MB | ~1,281 bytes/doc |
| **After fix**: this server (sled, zstd compression + `generate_id()`) | 2.6 MB | ~520 bytes/doc |
| Real CouchDB | 1.74 MB | ~365 bytes/doc |
| **Ratio (after fix)** | | CouchDB uses **~1.5x less disk** (down from ~3.5x) |

**The first measurement found a real, reported-plainly trade-off**: this
server was using 3.5x more disk than CouchDB for identical data. Two
root causes, both fixed in `db/src/storage.rs` and `db/src/main.rs`:

1. **sled's zstd compression was available but off by default.** JSON
   text compresses well; enabling it (`sled::Config::use_compression(true)`,
   `compression` Cargo feature) was the larger of the two wins.
2. **Every document write did three separate small sled writes**: the
   document itself, an entry in the `_changes` sequence log, and a
   hand-rolled read-modify-write on a counter stored in its own tree
   just to generate that sequence number. That counter tree was pure
   overhead — sled already has `Db::generate_id()`, a lock-free counter
   that batches ids in memory and persists a checkpoint only
   occasionally rather than writing to disk on every call. Replacing the
   counter tree with `generate_id()`, and deriving `current_seq()` by
   reading the highest key already in the sequence log instead of
   maintaining a redundant separate value, removed a full extra write
   per document.

Trade-off of fix #1: write throughput dropped from ~56,800 to ~29,940
docs/sec (compression costs CPU) — still ~8x faster than CouchDB, so a
clearly worthwhile trade. The remaining ~1.5x gap versus CouchDB's
purpose-built B-tree storage engine is a reasonable place to stop for
now; closing it further would mean not re-storing a document's full
revision-tree structure (parent pointers, deletion flags) as JSON on
every write, which is a larger, riskier storage-format change — logged
in `doc/open-questions.md` as a candidate for later, not attempted here.

Verified this didn't regress anything: full unit suite (14 tests), both
PouchDB integration tests, the load test, and the differential test
against real CouchDB all still pass after the change.

## Summary for a one-line pitch

> A ~4.4 MB single-file server, ~52x smaller to install than real
> CouchDB's ~229 MB Erlang runtime, running the exact protocol surface a
> real PouchDB client uses — verified byte-for-byte compatible with real
> CouchDB's conflict-resolution behavior (see USAGE.md §5), ~8x faster on
> bulk writes in this benchmark, modestly faster on reads, and within
> ~1.5x of CouchDB's disk usage per document (down from ~3.5x after a
> same-day fix) — every number here from an actual measured run, gaps
> and all, not cherry-picked.
