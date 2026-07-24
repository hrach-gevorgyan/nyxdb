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
| **What you ship/install** | One ~2.76 MB executable (`couchdb-clone.exe`) | Full install: Erlang/OTP runtime + ICU + a bundled Lucene-based search engine (`nouveau`) + CouchDB itself |
| **Total size** | 2.76 MB | 229 MB |
| **Ratio** | — | **~83x larger** |

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
| This server | 88ms | ~56,800 docs/sec |
| Real CouchDB | 1,477ms | ~3,385 docs/sec |
| **Ratio** | | **~16.8x faster** |

This gap is expected, not a fluke — CouchDB's write path does more per
document by design (its own MVCC B-tree updates, view group bookkeeping
hooks, replication-relevant metadata) that this server's much smaller
scope skips entirely. It is not evidence that sled is inherently 16x
faster than CouchDB's storage engine in general.

## Read throughput

200 sequential single-document `GET` requests (i.e. round-trip latency,
not raw server-side processing time — dominated by HTTP/TCP overhead on
localhost either way):

| | Time | Avg latency |
|---|---|---|
| This server | 2,011ms | 10.05ms/req |
| Real CouchDB | 2,346ms | 11.73ms/req |
| **Ratio** | | **~1.17x faster** |

Much closer than the write benchmark — reads are dominated by
per-request HTTP overhead on both servers, not by storage-engine
differences, so this is a much weaker signal than the write number
above.

## On-disk size (the honest surprise)

Same 5,000 documents, disk space actually used:

| | Size | Per doc |
|---|---|---|
| This server (sled) | 6.1 MB | ~1,281 bytes/doc |
| Real CouchDB | 1.74 MB | ~365 bytes/doc |
| **Ratio** | | **CouchDB uses ~3.5x less disk** |

**This is a real trade-off, not spin-worthy, so it's reported plainly.**
The likely cause: every write here re-serializes the *entire* revision
tree (all history for that doc, as a JSON-encoded `HashMap`) rather than
appending just the new revision — see `db/src/storage.rs::put_tree`.
CouchDB's actual B-tree storage engine is far more space-efficient at
this by design. This is a legitimate area for future optimization (not
started — see `doc/open-questions.md`), not a claimed advantage.

## Summary for a one-line pitch

> A ~2.76 MB single-file server, ~83x smaller to install than real
> CouchDB's ~229 MB Erlang runtime, running the exact protocol surface a
> real PouchDB client uses — verified byte-for-byte compatible with real
> CouchDB's conflict-resolution behavior (see USAGE.md §5), roughly 17x
> faster on bulk writes in this benchmark, and modestly faster on reads —
> at the cost of ~3.5x more disk space per document, a known and
> reported trade-off rather than a hidden one.
