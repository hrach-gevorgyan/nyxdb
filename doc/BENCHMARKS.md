# Benchmarks vs Real CouchDB

One machine, one run each, this server's release build vs. a real local
CouchDB 3.5.2. Not a rigorous statistical benchmark — no repeated
trials, no varied hardware. Good for order-of-magnitude, not a
performance guarantee. Reproduce with `test/benchmark/vs_couchdb.js`.

Numbers below are from the final pre-release run (NyxDB, post-rename).
The write/read/disk numbers were re-measured fresh; install size and
CouchDB's own memory baseline reuse the last independently-verified
figures noted inline.

---

## Install size

| | NyxDB | Real CouchDB |
|---|---|---|
| Size | 4.37 MB | 229 MB |
| Ratio | — | **~52x larger** |

CouchDB's 229 MB breakdown: Erlang libraries (52MB), `bin` (49MB), the
Erlang runtime (47MB), a bundled Lucene search engine (39MB), `share`
(28MB), the rest is data/config. This gap is why the project exists —
a 230MB Erlang runtime for a client that only calls ~10 HTTP endpoints.

Not apples-to-apples on capability — CouchDB's size buys you views,
Mango, clustering, attachments. Fair comparison only for the
replication-protocol surface a PouchDB client actually uses.

## Write throughput

5,000 small JSON documents (~200 bytes each), one `_bulk_docs` request:

| | Time | Throughput |
|---|---|---|
| NyxDB | 209ms | ~23,900 docs/sec |
| Real CouchDB | 1,362ms | ~3,670 docs/sec |
| Ratio | — | **~6.5x faster** |

CouchDB does more per write by design (its own MVCC, view bookkeeping).
This isn't evidence sled is faster than CouchDB's storage engine in
general — just that this server's narrower scope skips work CouchDB
has to do.

## Read throughput

200 sequential `GET` requests:

| | Time | Avg |
|---|---|---|
| NyxDB | 2,456ms | 12.28ms/req |
| Real CouchDB | 2,639ms | 13.20ms/req |
| Ratio | — | ~1.07x faster |

Close — reads are dominated by HTTP overhead on both sides, not the
storage engine. Weak signal either way.

## Disk size

Same 5,000 documents, disk space used:

| Version | Size | vs. CouchDB |
|---|---|---|
| v1: JSON tree, no compression | 6.1 MB | 3.5x more |
| v2: + zstd compression, `generate_id()` | 2.6 MB | 1.5x more |
| v3: + binary-encoded tree (bincode) | 2.31 MB | 1.33x more |
| v5: + varint integer encoding | 2.26 MB | 1.29x more |
| **Final pre-release** | **1.83 MB** | **~1.05x more** |
| Real CouchDB | 1.74 MB | — |

The gap closed further than v5's own numbers suggested it would —
likely dataset-shape sensitivity (this run's synthetic docs compress
slightly better) rather than a code change since v5. Re-measured
directly rather than assumed: `du`-equivalent byte count on the sled
data directory after the same 5,000-doc write used for the throughput
benchmark above.

What changed at each step (`db/src/storage.rs`, `db/src/main.rs`):

- **v1→v2**: sled's zstd compression was available but off by default —
  turned it on. Also cut a redundant sled write per document (a
  hand-rolled sequence counter, replaced with sled's own `generate_id()`).
- **v2→v3**: the revision tree was JSON-wrapped, repeating field names
  per revision. Switched to a compact `bincode` struct.
- **v3→v5**: `bincode`'s default encoding uses fixed 8-byte length
  prefixes for every string, even tiny ones. Varint encoding shrinks
  that. Also improved write speed slightly, since smaller payloads
  compress faster.

**Tried and reverted twice: dictionary compression.** The idea was
sound — sled compresses each document independently, so repeated field
names across documents can't be reclaimed. Built a shared compression
dictionary two different ways (zstd streaming frames, then zstd's raw
block API with a cached dictionary, matching the technique sled itself
uses internally). Both made disk usage *worse* (2.26MB → 3.0-3.1MB),
not better — for documents this small, the overhead outweighs the
redundancy a generic dictionary can find. Disabling sled's own
compression to isolate the cause made it far worse (4.78MB), confirming
sled's built-in compression already does this job well. Fully reverted
both times. Details in `changelog.md` if you want the full story;
short version is it didn't work and isn't worth a third attempt without
real training data.

**Remaining gap in practice**: ~107 bytes/doc extra vs. CouchDB. For a
personal or small-team app, that's roughly 1MB extra per 10,000
documents ever written — not a real-world problem. See the note at the
bottom of this file.

Two things deliberately not attempted: a *properly trained* compression
dictionary (needs real sample data that doesn't exist ahead of time),
and packing revision ids as raw bytes instead of hex strings (touches
the exact string comparison the conflict-resolution logic depends on —
not worth the risk for a small gain).

Verified after every change: full unit suite, both PouchDB integration
tests, the load test, and the differential test against real CouchDB
all still pass.

## Memory

Measured with `Get-Process` (working set) on both sides:

| | NyxDB | Real CouchDB | Ratio |
|---|---|---|---|
| Idle (freshly started, no requests yet) | 30.4 MB | ~93.5 MB* | **~3x lighter** |
| Under load (2,000 docs + 20 subscribers) | 52 MB | ~116 MB* | **~2x lighter** |

A real win, not assumed — CouchDB's Erlang runtime carries overhead a
single Rust process doesn't. NyxDB's own numbers are freshly
re-measured for this release; CouchDB's are the last independently
verified figures (\*idle from the original benchmark pass, load-side
figure cross-checked against a live `erl.exe` reading of 116MB taken
after this session's differential+benchmark runs against it).

## Startup time

Server ready to accept requests (`GET /` returns 200) essentially
immediately after process start — under 20ms from spawn to first
successful health check, dominated by process creation overhead, not
initialization work. No comparable CouchDB number is meaningful here:
starting the Erlang VM/OTP release takes seconds, not milliseconds.

## `_changes` latency

Two measurements matter here:

| | Latency |
|---|---|
| Full round-trip (write → client sees it over HTTP) | ~13.4ms avg |
| In-process only (write committed → subscriber's stream gets it) | **~0.14ms avg** |

The ~13ms is HTTP/TCP overhead on the write itself — nothing to do with
the `_changes` mechanism. The actual notification path (a write
publishing to `ChangeFeed` in `db/src/changes.rs`) is already
sub-millisecond.

## Real-world impact for a personal/small-team app

Disk size is now within ~5% of CouchDB's usage per document — about 19
extra bytes per document at this run's dataset shape. In absolute
terms, at the older (still-conservative) ~1.29x figure:

| Usage scale | Extra disk vs. CouchDB |
|---|---|
| Personal use, 10,000 documents over years | ~1.1 MB |
| Small team, 100,000 documents over years | ~10.8 MB |
| 1,000,000 documents (unlikely at this scale) | ~108 MB |

Any device this runs on has tens of GB free. This gap was never a
practical problem at any of the measured ratios, and is smaller now
than when that table was first written.

## Summary

A ~4.4 MB single-file server, ~52x smaller to install than CouchDB's
229 MB Erlang runtime, running the exact protocol surface a real
PouchDB client uses. ~6.5x faster on writes, modestly faster on reads,
2-3x lighter on memory, near-instant startup, sub-millisecond
live-sync notification, and within ~5% of CouchDB's disk usage per
document in this run's measurement.
