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
| **What you ship/install** | One ~4.23 MB executable (`couchdb-clone.exe`, zstd statically linked in) | Full install: Erlang/OTP runtime + ICU + a bundled Lucene-based search engine (`nouveau`) + CouchDB itself |
| **Total size** | 4.23 MB | 229 MB |
| **Ratio** | — | **~54x larger** |

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

This is the number that motivated the whole project: a ~230 MB Erlang
runtime is a lot to vendor into an app just for optional sync, when a
real client only ever calls ~10 HTTP endpoints.

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
| This server | 212ms | ~23,585 docs/sec |
| Real CouchDB | 1,530ms | ~3,268 docs/sec |
| **Ratio** | | **~7.2x faster** |

Slower than an earlier measurement of this server (88ms/~56,800 docs/sec,
before any of the disk-size fixes below) — each disk-size fix traded
some write-side CPU for less disk usage: zstd compression cost the first
chunk of throughput, binary revision-tree encoding cost a bit more (see
below). Still comfortably faster than CouchDB throughout. This gap vs.
CouchDB is expected, not a fluke either way — CouchDB's write path does
more per document by design (its own MVCC B-tree updates, view group
bookkeeping hooks, replication-relevant metadata) that this server's
much smaller scope skips entirely. It is not evidence that sled is
inherently faster than CouchDB's storage engine in general.

## Read throughput

200 sequential single-document `GET` requests (i.e. round-trip latency,
not raw server-side processing time — dominated by HTTP/TCP overhead on
localhost either way):

| | Time | Avg latency |
|---|---|---|
| This server | 2,250ms | 11.25ms/req |
| Real CouchDB | 2,422ms | 12.11ms/req |
| **Ratio** | | **~1.08x faster** |

Much closer than the write benchmark — reads are dominated by
per-request HTTP overhead on both servers, not by storage-engine
differences, so this is a much weaker signal than the write number
above.

## On-disk size — found a real gap, then closed most of it

Same 5,000 documents, disk space actually used, measured after each of
three successive changes:

| | Size | Per doc | vs. CouchDB |
|---|---|---|---|
| **v1 (original)**: JSON-encoded tree, no compression, hand-rolled seq counter | 6.1 MB | ~1,281 bytes/doc | 3.5x more |
| **v2**: + zstd compression + `generate_id()` | 2.6 MB | ~520 bytes/doc | 1.5x more |
| **v3**: + binary-encoded revision tree (bincode, raw body bytes) | 2.31 MB (2,423,725 bytes) | ~485 bytes/doc | **1.33x more** |
| Real CouchDB | 1.74 MB (1,827,238 bytes) | ~365 bytes/doc | — |

**v1 → v2** found a real, reported-plainly trade-off: this server was
using 3.5x more disk than CouchDB for identical data. Two root causes,
fixed in `db/src/storage.rs`/`db/src/main.rs`:

1. **sled's zstd compression was available but off by default.** JSON
   text compresses well; enabling it (`sled::Config::use_compression(true)`,
   `compression` Cargo feature) was the larger of the two wins.
2. **Every document write did three separate small sled writes**: the
   document itself, an entry in the `_changes` sequence log, and a
   hand-rolled read-modify-write on a counter stored in its own tree
   just to generate that sequence number. Replaced with sled's own
   `Db::generate_id()` (lock-free, batches ids in memory, persists a
   checkpoint only occasionally) and `current_seq()` reading the
   sequence log's own highest key instead of separate redundant state.

**v2 → v3**, a smaller, more deliberate follow-up: JSON-wrapping the
revision tree (`{"nodes":{"1-hash":{"parent":...,"deleted":...,"body":...}}}`)
repeats field names and the hex rev-id string for every revision — pure
overhead sled's per-value compression can't fully reclaim, since each
value is compressed independently with no cross-document dictionary to
lean on. Replaced with a compact `bincode`-encoded struct
(`StoredTree`/`StoredNode` in `storage.rs`), keeping the document body
as pre-serialized raw JSON bytes rather than asking `bincode` to
understand arbitrary JSON shapes (which it can't —
`DeserializeAnyNotSupported`, the same trap hit once already in Phase 0).
Result: a further ~11% reduction (2.6MB → 2.31MB), closing the gap from
1.5x to 1.33x.

**A dead end worth reporting, not hiding**: `suggestions.md` (an
external optimization proposal, not kept in this repo — its useful
content is fully captured here and in `doc/changelog.md`) suggested
using a faster zstd
compression level to recover write throughput. Tested directly by
making the level configurable (`COUCHDB_CLONE_COMPRESSION_LEVEL` env
var) and measuring level 1 vs. the default — **write speed and disk
size were both statistically unchanged** (166ms/2.6MB at level 1 vs.
167ms/2.6MB at default). For documents this small (~200-400 bytes each,
compressed independently), zstd's effort-level knob doesn't have enough
material to work with regardless of setting; the fixed per-call
overhead of invoking compression at all dominates, not the effort level.
The knob is left in place (harmless, might matter for larger documents
in other use cases) but it is not the throughput fix `suggestions.md`
assumed it would be.

Trade-off across both real fixes: write throughput dropped from
~56,800 → ~29,940 (zstd) → ~23,585 docs/sec (binary encoding) — each
step traded some write-side CPU for less disk. Still **~7.2x faster
than CouchDB** at the end of both changes, so clearly worth it. The
remaining 1.33x gap versus CouchDB's purpose-built B-tree storage
engine is a reasonable place to stop; closing it further would mean
either dictionary-based compression (bypassing sled's built-in
per-value compression entirely to share a trained dictionary across
documents — real engineering effort, not a config flag) or further
tightening the binary rev-id encoding (e.g. splitting `"1-<hex>"` into
a raw `u64` generation + fixed-width hash bytes instead of a string) —
both logged in `doc/open-questions.md` as candidates, not attempted here
since the remaining gap is now small relative to the effort to close it.

Verified none of this regressed anything: full unit suite (14 tests),
both PouchDB integration tests, the load test, and the differential
test against real CouchDB all still pass after each change.

## Memory and `_changes` latency — correcting two fabricated numbers

`suggestions.md` also claimed a "current" baseline of ~15MB RSS and
~5–10ms `_changes` latency, targeting <10MB and <1ms respectively.
**Neither baseline number was ever actually measured in this repo before
now** — both were invented. Measuring them properly:

| Metric | `suggestions.md` claimed "current" | Actually measured |
|---|---|---|
| Idle memory (working set) | ~15 MB | **~30.8 MB** |
| Memory under load (30 subscribers, 5,000-doc batch) | (no figure given) | **~60.7 MB peak, ~56.5 MB settled** |
| `_changes` latency, full HTTP round-trip (write → subscriber sees it over the wire) | ~5–10ms | **~13.4ms avg** (p50 15ms, p95 20ms) |
| `_changes` latency, isolated propagation only (write already ACKed → subscriber's stream receives it) | *(not distinguished from the above)* | **~0.14ms avg** (p50 0ms, p95 1ms) — already meets the "<1ms" target |

The memory numbers mean `suggestions.md`'s "<10MB active" target was
never reachable — it's below the real *idle* baseline, let alone under
load, and no realistic amount of zero-copy JSON parsing changes that:
the floor is set by the tokio runtime, axum's routing tables, and
sled's own page cache, not by this server's own allocations.

The latency finding is more interesting: **the specific thing
`suggestions.md` proposed to fix (an in-memory ring buffer for
`_changes?since=X`) targets a bottleneck that mostly doesn't exist for
the actual `live:true` continuous-feed case.** The in-process
notification path (a write publishing to the `ChangeFeed` broadcast
channel in `db/src/changes.rs`, and a connected continuous subscriber
receiving it) is already sub-millisecond — it's plain in-memory Rust
code. The ~13ms a client actually experiences is HTTP/TCP round-trip
overhead on the *write* itself, which no `_changes`-side caching would
touch. Measured with `test/benchmark/changes_latency.js`.

**Neither of these was chased further** — the memory target was never
realistic even before measuring, and the latency target turned out to
already be met once measured correctly (for the mechanism that actually
matters for `live:true` sync, as opposed to the full-HTTP-round-trip
framing the target conflated it with).

## Summary for a one-line pitch

> A ~4.2 MB single-file server, ~54x smaller to install than real
> CouchDB's ~229 MB Erlang runtime, running the exact protocol surface a
> real PouchDB client uses — verified byte-for-byte compatible with real
> CouchDB's conflict-resolution behavior (see USAGE.md §5), ~7x faster on
> bulk writes in this benchmark, modestly faster on reads, an
> already-sub-millisecond live-sync notification path, and within ~1.33x
> of CouchDB's disk usage per document (down from ~3.5x across two
> rounds of real fixes) — every number here from an actual measured run,
> gaps, dead ends, and corrected fabrications all included, not
> cherry-picked.
