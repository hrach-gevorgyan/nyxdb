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
| This server | 191ms | ~26,178 docs/sec |
| Real CouchDB | 1,436ms | ~3,482 docs/sec |
| **Ratio** | | **~7.5x faster** |

Dipped as low as 88ms/~56,800 docs/sec (before any disk-size fixes) down
to 212ms/~23,585 docs/sec (after zstd compression + binary tree
encoding), then recovered slightly to the number above after switching
bincode to varint encoding (below) — smaller encoded payloads compress
and write faster, a rare case where the size fix also helped speed
instead of costing it. Still comfortably faster than CouchDB throughout.
This gap vs. CouchDB is expected, not a fluke either way — CouchDB's
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
| This server | 2,611ms | 13.05ms/req |
| Real CouchDB | 2,841ms | 14.21ms/req |
| **Ratio** | | **~1.09x faster** |

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
| **v3**: + binary-encoded revision tree (bincode, raw body bytes) | 2.31 MB (2,423,725 bytes) | ~485 bytes/doc | 1.33x more |
| **v5**: + varint integer/length encoding (v4 attempted, reverted — see below) | 2.26 MB (2,364,966 bytes) | ~473 bytes/doc | **1.29x more** |
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

**v3 → attempted v4, reverted: dictionary compression made things worse
on both axes.** The theory was sound — sled compresses each value
independently with no shared context, so cross-document redundancy
(repeated JSON field names across every document) can't be reclaimed by
per-value compression alone. Implemented it: a static ~2KB "raw content"
dictionary of common JSON/CouchDB vocabulary (not formally trained via
`ZDICT_trainFromBuffer`, since there's no sample corpus available ahead
of time for an arbitrary app's documents), applied via `zstd`'s
dictionary API at the storage layer. Two real problems surfaced by
actually measuring it, not by reasoning about it in advance:

1. **First attempt (rebuilding the dictionary's compression tables on
   every single write) dropped throughput from ~23,585 to ~1,942
   docs/sec** — over 10x. Fixed by preparing the dictionary once
   (`EncoderDictionary`/`DecoderDictionary`, cached in a `OnceLock`) and
   reusing it via `with_prepared_dictionary` instead of rebuilding per
   call.
2. **Even with the cached dictionary, disk size got *worse*, not
   better: 2.31MB → 3.00MB, a 29.8% regression** — and throughput was
   still only ~10,309 docs/sec, less than half the pre-dictionary rate.
   The likely cause: each zstd streaming frame carries fixed overhead
   (magic number, frame header, a dictionary ID reference, since a
   dictionary was used) — for genuinely tiny payloads (~200-400 bytes
   per document), that fixed per-frame cost outweighs whatever
   cross-document redundancy an untrained, generic dictionary manages
   to capture. The technique likely needs either much larger payloads
   per compressed unit (batching many documents into one frame, a
   bigger architecture change) or a properly *trained* dictionary from
   real sample data (which requires the sample data to exist first) to
   pay off — neither of which this attempt did.

**Reverted entirely** rather than kept behind a flag — it has no upside
in its current form, only cost, so there's nothing worth preserving.
This is exactly the outcome honest measurement is for: a plausible,
well-reasoned idea that made things worse in practice, caught before it
shipped rather than assumed to be an improvement because the reasoning
sounded right.

**v3 → v5** (skipping v4, the reverted dictionary attempt): after that
failure, reconsidered the actual root cause instead of reaching for
another compression trick. `bincode::serialize`/`deserialize` — the
top-level convenience functions used since v3 — turned out to use
**fixed-width 8-byte integers and 8-byte length prefixes for every
`String`/`Vec`**, confirmed in bincode's own source
(`config/legacy.rs`), regardless of how small the actual value is. A
revision id like `"1-a1b2c3d4e5f67890"` was paying an 8-byte length
prefix to say "this string is 18 bytes long." Switching to
`DefaultOptions::new().with_varint_encoding()` (`bincode_options()` in
`storage.rs`) shrinks every one of those prefixes and small integers,
with **no change to what's actually stored** — pure encoding-density
win, and notably lower-risk than the reverted dictionary attempt, since
it never touches the revision-hash string that winner-picking's
tiebreak compares byte-for-byte (a repacking scheme that changed that
representation was considered and deliberately not attempted, given the
correctness risk to a differential-tested code path for a modest
expected payoff). Result: 2.31MB → 2.26MB, **and** write throughput
*improved slightly* (~23,585 → ~26,178 docs/sec) — smaller encoded
payloads compress and write faster, a rare case where a size fix helped
speed instead of costing it. Verified reproducible (identical byte
count, 2,364,966, across two independent runs).

Trade-off across the three fixes that stuck (zstd compression, binary
tree encoding, varint bincode config): write throughput went
~56,800 → ~29,940 → ~23,585 → ~26,178 docs/sec — still **~7.5x faster
than CouchDB**. The remaining 1.29x disk gap versus CouchDB's
purpose-built B-tree storage engine is where this stops for now — the
one untried candidate (a properly trained dictionary with batched/larger
compression units, since the naive per-value dictionary approach
already failed) is logged in `doc/open-questions.md`, not attempted
given the gap is now small relative to the effort and risk.

Verified none of the changes that were kept regressed anything: full
unit suite (14 tests), both PouchDB integration tests, the load test,
and the differential test against real CouchDB all still pass.

## Memory — measured on both sides, not assumed

The original disk-size investigation left one gap: memory had only ever
been measured for this server, never for real CouchDB, so "is memory
also a problem" was an open question rather than a measured fact.
Fixed — both measured with `Get-Process` (working set, the physically
resident metric):

| | This server | Real CouchDB (`erl.exe`) | Ratio |
|---|---|---|---|
| Idle | ~30.8 MB | ~93.5 MB | **this server ~3x lighter** |
| Under load (5,000-doc batch + 30 concurrent `feed=continuous`/continuous-changes subscribers) | ~56–60 MB | ~115.7 MB | **this server ~2x lighter** |

Memory is not a losing metric — it's a winning one, by a comparable
margin to the other wins. CouchDB's Erlang/OTP runtime carries real
baseline overhead (the BEAM VM, its own scheduler, NIF/driver
infrastructure) that a single-process Rust binary with a modest tokio
runtime doesn't pay. This closes out the "also need to be light"
question with a real number instead of an assumption in either
direction.

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
> CouchDB's conflict-resolution behavior (see USAGE.md §5), ~7.5x faster
> on bulk writes, modestly faster on reads, 2-3x lighter on memory both
> idle and under load, an already-sub-millisecond live-sync notification
> path, and within ~1.29x of CouchDB's disk usage per document (down
> from ~3.5x across three rounds of real fixes, one deliberately
> reverted after measurement showed it made things worse) — every number
> here from an actual measured run, gaps, dead ends, and corrected
> fabrications all included, not cherry-picked.
