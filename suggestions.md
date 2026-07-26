# High-Performance CouchDB Rust Replica: Optimization & Benchmark Strategy

This document outlines the architectural scenarios, AI implementation prompts, risk verification tests, and benchmark targets for creating an ultra-lightweight, high-speed CouchDB-compatible storage engine written in Rust using `sled`.

---

## Performance Benchmark Objectives

| Metric | Official CouchDB 3.5.2 | Rust Replica (Current) | Target Goal | Strategy / Mechanism |
| :--- | :--- | :--- | :--- | :--- |
| **Install / Binary Size** | ~229 MB | **4.42 MB** | **< 5.0 MB** | Zero Erlang VM dependencies; minimal crate stack |
| **Write Throughput** | 3,759 docs/sec | 29,940 docs/sec | **> 50,000 docs/sec** | Parallel Rayon CPU worker pool + Zstd Fast Level |
| **Disk Size (5,000 docs)** | 1.74 MB | 2.60 MB | **< 1.50 MB** | Trained Zstd dictionary + Binary revision tree packing |
| **`_changes` Sync Latency** | ~10–20 ms | ~5–10 ms | **< 1.0 ms** | In-memory ring buffer for recent change sequences |
| **Memory Footprint (RSS)** | > 150 MB | ~15 MB | **< 10 MB** | Zero-copy JSON parsing & direct `sled::IVec` slice usage |

---

## Scenario 1: Squeezing Disk Storage (< 1.5 MB Target Footprint)

### AI Instruction Prompt

```text
CONTEXT:
We are building a lightweight, local-first CouchDB-compatible storage engine in Rust using `sled`. Our goal is to beat CouchDB's disk footprint on 5,000 documents (< 1.5 MB).

TASK:
Refactor document storage and revision tree serialization to minimize disk overhead.

IDEA:
1. Stop storing JSON strings for revision tree metadata (`_rev`, `_revisions`, branch history).
2. Train and apply a static Zstd dictionary for small JSON payloads.
3. Separate metadata key-spaces into dedicated `sled::Tree` handles.

IMPLEMENTATION STEPS:
1. Split storage into two trees: `db.open_tree("docs")` for document bodies and `db.open_tree("revs")` for binary revision histories.
2. Define a compact binary schema for revision trees using `postcard` or `bincode` instead of serde_json.
3. Integrate `zstd::dict` training logic for JSON payloads:
   - Use a pre-trained 4KB compression dictionary during `zstd` encode/decode cycles on the `docs` tree.
4. Ensure `sled` keys use big-endian fixed-byte keys or prefix-friendly encoding for optimal B-Tree page compaction.

RISKS & TESTS TO CHECK:
- Test: Verify zero loss of determinism in CouchDB `_rev` winning-branch resolution after binary conversion.
- Risk: Dictionary mismatch errors. Add a unit test verifying that doc decompression falls back safely if payload wasn't dictionary-encoded.
- Test: Verify tree metadata cleanup when documents are purged or compacted.

BENCHMARK IDEA:
- Write 5,000 documents (approx 1KB each).
- Measure total disk directory size of the `sled` data folder on disk.
- Target Metric: Total disk size < 1.5 MB (beating CouchDB's 1.74 MB).

--- 

##Scenario 2 
CONTEXT:
Inline Zstd compression drops bulk write throughput from 56,800 to 29,940 docs/sec because CPU compression blocks the storage thread.

TASK:
Implement asynchronous parallel compression and micro-batching for bulk writes.

IDEA:
Move Zstd compression off the storage write path onto a dedicated CPU worker pool, and flush compressed blocks in micro-batches to `sled`.

IMPLEMENTATION STEPS:
1. Set up a thread pool (e.g., using `rayon` or `tokio::task::spawn_blocking`).
2. Create an in-memory channel/buffer that accepts incoming bulk payload batches.
3. Compress document payloads in parallel across worker threads using fast Zstd compression level (Level 1 or `--fast`).
4. Collect compressed payloads into a `sled::Batch` and commit to disk in micro-batches (e.g., every 2ms or 100 items).

RISKS & TESTS TO CHECK:
- Risk: Out-of-order writes or race conditions during rapid updates to the same document ID.
- Test: Add a concurrency unit test that sends 1,000 rapid updates to the SAME `_id` across 10 concurrent threads; verify final state correctness.
- Test: Verify memory limits so the micro-batch channel doesn't grow infinitely under write heavy load (backpressure handling).

BENCHMARK IDEA:
- Execute a 50,000 document bulk write with Zstd compression enabled.
- Target Metric: Maintain write speed > 50,000 docs/sec while retaining < 1.8 MB disk usage.


---

##Scenario 3 
CONTEXT:
CouchDB sync latency is bottlenecked by disk lookups on `_changes?since=X` queries and string-based revision hash comparisons during `_revs_diff`.

TASK:
Build an in-memory acceleration layer for change feeds and revision checking.

IDEA:
Use an in-memory ring buffer for recent change sequences and integer pre-hashes for revision branches.

IMPLEMENTATION STEPS:
1. Implement a lock-free in-memory ring buffer (storing the last 1,000 sequence IDs + `_id` + `_rev`).
2. On `_changes?since=X` requests:
   - If `X` is present in the ring buffer, return changes directly from RAM without hitting `sled`.
   - If `X` is older than the buffer, fall back to disk read from `sled::Tree("seq")`.
3. Pre-compute 64-bit integer hashes (`xxhash` / `ahash`) for revision strings to speed up `_revs_diff` comparisons during replication sync negotiation.

RISKS & TESTS TO CHECK:
- Risk: Ring buffer state becoming desynchronized with disk state during abrupt process restarts.
- Test: Simulate abrupt process shutdown after 500 writes; verify that upon cold boot, the ring buffer accurately re-hydrates from `sled`.
- Test: Verify boundary conditions when client asks for `since=X` where X is exactly at the edge of the ring buffer capacity.

BENCHMARK IDEA:
- Connect 5 concurrent HTTP clients polling `_changes?feed=continuous`.
- Insert 1,000 documents one by one and measure round-trip time from insert to subscriber notification.
- Target Metric: Mean sync propagation latency < 1 ms.

---
##Scenario 4
CONTEXT:
Memory footprint during continuous sync should stay ultra-low to allow execution on edge devices and background mobile sync tasks.

TASK:
Reduce allocations during document serialization and HTTP processing using zero-copy patterns.

IDEA:
Replace standard JSON parsing allocations with zero-copy decoding (`simd-json` or `serde` zero-copy `Cow<str>`).

IMPLEMENTATION STEPS:
1. Audit JSON handling logic across all HTTP endpoints.
2. Refactor HTTP request body parsing to use zero-copy buffers (`bytes::Bytes` slice references instead of allocating intermediate `String`s).
3. Ensure value retrievals from `sled` leverage `IVec` direct slice references without copying bytes into new `Vec<u8>` arrays unless necessary.

RISKS & TESTS TO CHECK:
- Test: Run memory profiling (e.g., `heaptrack` or `dtrace`) during a 20,000 document sync cycle.
- Risk: Memory leaks in long-running streaming HTTP connections.
- Test: Keep 100 continuous long-poll connections open for 10 minutes while pushing updates; verify peak RSS memory stays flat.

BENCHMARK IDEA:
- Measure total RSS (Resident Set Size) memory under active synchronization.
- Target Metric: Idle memory < 5 MB; Active under load < 12 MB.