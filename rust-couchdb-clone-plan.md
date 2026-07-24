# Building a Minimal, PouchDB-Compatible Sync Server in Rust

**Status:** brainstorm / reference document, not an active project.
**Origin:** written while exploring whether Offlog (a Svelte + PouchDB
local-first task manager) could someday replace its ~150MB vendored
Erlang/CouchDB binary with a small Rust-native server implementing only
the protocol surface PouchDB actually uses. This document generalizes
that into a standalone reference for a **new, separate project** — it is
grounded in a real, working app's actual usage (concrete config, code,
call sites) rather than written abstractly, so treat the concrete
examples as "here is proof this is a real, bounded scope," not as
something the new project must literally replicate.

---

## 1. Why this is worth doing (and why it's *not* crazy)

### 1.1 The case for it
Local-first apps built on PouchDB (a full IndexedDB/LevelDB-backed
database with built-in multi-master replication) get **optional sync for
free** the moment they point at any server that speaks CouchDB's
replication protocol. That protocol is:
- **Well-specified and stable** — it hasn't meaningfully changed in
  years; PouchDB and CouchDB are maintained by overlapping communities
  and both treat protocol compatibility as sacred.
- **A small, bounded subset of full CouchDB.** A real client (PouchDB's
  own `http` adapter) only ever calls about eight endpoints. You are not
  reimplementing "a document database with MapReduce views, Mango
  indexes, clustering, and a plugin system" — you're reimplementing "a
  document store with one very specific replication protocol on top."
- **Genuinely decoupled from the storage engine underneath.** CouchDB's
  own storage (originally per-database append-only B-trees, more
  recently backed by FoundationDB in CouchDB 4.x) is an implementation
  detail the protocol doesn't expose. Any Rust storage engine (sled,
  RocksDB, SQLite, or hand-rolled) is a legitimate substitute as long as
  it can answer the protocol's questions (given a doc id, what revisions
  exist; given a since-checkpoint, what changed).

### 1.2 The case against it (be honest about this)
- **Correctness bar is a data-loss/data-corruption bar, not a UI bug
  bar.** Sync protocols are exactly the kind of system where a subtle
  edge case (a 3-way conflict, a replication interrupted mid-batch, a
  revision-tree branch pruning bug) causes silent, hard-to-notice
  corruption weeks later, not a visible crash today. Budget real time
  for adversarial testing (see §6), not just "the demo works."
- **The revision-tree + deterministic-conflict-winner semantics are the
  actual hard part**, not the HTTP layer. CouchDB's winner-picking
  algorithm (longest branch wins, hash of the revision content as
  tiebreaker) is deterministic *within CouchDB's own implementation*,
  but a client library (PouchDB) was built and tested against that
  *exact* algorithm. A "close enough" reimplementation that occasionally
  disagrees with what PouchDB itself would have computed produces
  divergence that's invisible until two devices compare notes.
- **Real-world scope creep risk**: CouchDB's docs conflate "the
  replication protocol" with "views, Mango, `_security`, clustering,
  attachments compression, etc." It's easy to accidentally scope-creep
  into reimplementing all of CouchDB. Section 3 below is deliberately a
  hard, small boundary — resist adding anything not on that list until
  a real, concrete need shows up.

### 1.3 Precedent this isn't unprecedented
Several PouchDB-compatible server reimplementations already exist and
prove the protocol is genuinely clonable at a bounded scope:
- **CouchDB itself** (Erlang, reference implementation).
- **pouchdb-server** (Node.js — literally PouchDB's own server-mode,
  proves the protocol surface is small enough that PouchDB's *own*
  client-side code could be flipped around into a server).
- **Cloudant** (IBM's hosted CouchDB fork — same protocol, different
  storage/ops layer underneath).
- Various smaller community projects reimplementing subsets in other
  languages (Go, Python) for embedded/edge use cases similar to this one.

None of these are in Rust as far as is known at the time of writing —
that would be the interesting, non-redundant part of doing this.

---

## 2. What a real production app actually uses (grounded example)

This section is deliberately concrete — it's the evidence that "the
protocol surface a real app needs" is genuinely small, taken from a real
shipping app (not a toy example).

### 2.1 Server-side configuration that mattered
- Single database per deployment (`PUT /{db}`, idempotent — 412 if it
  already exists).
- HTTP Basic auth over plain HTTP (no TLS — this was a deliberate
  decision for a home-LAN-only deployment; **your new project should
  decide this explicitly, don't inherit it by default** — see §7).
- CORS needed to be explicitly enabled (`enable_cors=true`,
  `origins=*`, credentials allowed) because the client was a browser
  WebView on a different origin than the server.
- Admin credentials generated randomly per-install and written to a
  config file, not baked into any shared default — "admin party" (no
  auth at all) is CouchDB's insecure-by-default state and must be
  explicitly closed.

### 2.2 Client-side (PouchDB) usage
- **Exactly one plugin beyond core**: `pouchdb-find` (Mango query
  support) — and even that was used **only for local queries against
  the browser's own local database**, never against the remote server.
  This is an important scoping insight: **Mango/`_find` has nothing to
  do with the replication protocol** and can be left out of a v1 clone
  entirely if the goal is "sync works," with local Mango queries still
  working fine against PouchDB's own local storage regardless of what
  the remote server supports.
- **Sync call**: `db.sync(remoteDb, { live: true, retry: true })` — no
  filters, no `since` override, no custom `batch_size`. Plain
  bidirectional continuous replication. This is the common case; a v1
  server only needs to support this shape well.
- **No attachments** — many real apps never touch PouchDB's binary
  attachment support at all. If your new project's data model is
  JSON-only (no user-uploaded files stored as CouchDB attachments), you
  can defer `_attachments`/multipart MIME handling to a much later phase
  or skip it entirely.

### 2.3 Conflict handling — the part that actually needs care
- A client library expects `_conflicts` to appear on a fetched document
  (via `include_docs=true&conflicts=true` on `_all_docs`, or directly on
  a `GET /{db}/{id}?conflicts=true`), listing the losing revisions.
- Resolving a conflict from the client means explicitly `DELETE`-ing
  every losing revision by its exact rev id — the server does **not**
  auto-prune losing branches just because a new revision was written on
  top of the winner. Your server must preserve full revision-tree
  history (all branches) until a client explicitly removes them, not
  just track "current winner."
- The **winner-picking algorithm** itself: given a document's competing
  revision branches, the winner is deterministic — longest branch
  (highest generation number) wins; ties are broken by comparing the
  revision hash lexicographically (CouchDB documents this as
  `winning_revision`, described further in §4.4). **This must be
  bit-for-bit consistent with what PouchDB itself would compute**,
  because PouchDB's own local-vs-remote conflict math assumes the same
  algorithm on both ends.

### 2.4 Discovery/pairing is a separate, simpler concern
Finding the server's address and exchanging credentials (mDNS
advertisement, a short-lived pairing code, whatever mechanism your new
project uses) is **entirely orthogonal to the replication protocol
itself** — once a client has a URL + credentials, it just runs standard
HTTP replication against it. Don't conflate "how do two devices find each
other" with "what does the sync protocol look like" — they're
independently designable, and the discovery layer can be swapped out
without touching the protocol server at all.

---

## 3. The protocol surface — the actual scope boundary

This is the complete list of endpoints a real client (PouchDB's `http`
adapter) calls. If your server implements exactly these, correctly,
`db.sync()` and `db.replicate.to()/.from()` work against it with zero
client-side changes. Resist adding anything beyond this list without a
concrete reason.

| Endpoint | Purpose |
|---|---|
| `GET /` | Server identification (returns `{"couchdb":"Welcome", "version":"...", "uuid":"..."}`) — clients use this to fetch a stable server UUID. |
| `GET /{db}` | Database metadata (`doc_count`, `update_seq`, etc.) — used to decide replication direction/checkpoints. |
| `PUT /{db}` | Create database. |
| `GET /{db}/_local/{id}` / `PUT /{db}/_local/{id}` | **Replication checkpoints.** Each replication direction stores a small doc here recording "last seq I successfully replicated up to" — this is how `live:true` resumable sync works across restarts. `_local` docs are never replicated themselves and have no revision history (single-revision, last-write-wins). |
| `POST /{db}/_revs_diff` | Given `{docId: [rev1, rev2, ...]}`, return which of those revisions the server does NOT already have — this is how the client figures out what it actually needs to push, avoiding re-sending data the server already has. |
| `POST /{db}/_bulk_docs` | Push a batch of document revisions (with explicit `_rev` history via `new_edits:false` during replication — the pushing side sends its own revision tree, not "generate a new one"). |
| `POST /{db}/_bulk_get` | Given a list of `{id, rev}` pairs, fetch the actual document bodies in one batched round-trip (replaces N individual `GET`s). |
| `GET /{db}/_changes` | **The core "what changed" feed.** Supports `since` (a checkpoint/sequence token), `style=all_docs` (include all leaf revisions, not just the winner — needed so conflicts replicate too), and three delivery modes: normal (one JSON response), `feed=longpoll` (holds the connection open until something changes or a timeout, then returns), `feed=continuous` (a persistent connection streaming newline-delimited JSON as changes happen — this is what powers `live:true`). |
| `GET /{db}/{id}` (+ `?conflicts=true&revs=true&open_revs=...`) | Fetch a specific document, optionally with conflict/revision-history metadata. |
| `POST /{db}/_ensure_full_commit` | A legacy durability-flush call some replicators still send at the end of a batch — can be a no-op that returns success if your storage is already durable per-write. |
| `GET /_session` (optional, if doing cookie auth) | Session-based auth alternative to HTTP Basic — only needed if you want browser-friendly cookie auth instead of Basic auth over every request. |

**Deliberately excluded from v1 scope** (per the real-world usage in §2):
Mango `_find`, MapReduce views (`_design` docs, `_view`), attachments,
`_security` docs, clustering/sharding, compaction endpoints, database
listing (`_all_dbs`), replication-triggered-by-server (`_replicate`
endpoint — assume client-driven replication only, i.e. the app's own
PouchDB always initiates, the server never pushes to a remote on its
own).

---

## 4. Core data structures to get right

### 4.1 Revision id format
A revision id is `{generation}-{hash}`, e.g. `3-a1b2c3...`. Generation is
a plain incrementing integer per branch depth; hash is (in real CouchDB)
an MD5 of a canonical serialization of the revision's content + its
parent rev + attachments stub info. **You do not need MD5-compatibility
with real CouchDB** unless you need byte-for-byte interop with an actual
CouchDB deployment (e.g. migrating an existing database) — for a from-
scratch server, any deterministic hash function works as long as it's
stable (same content ⇒ same hash) and collision-resistant enough
(SHA-256 truncated is a reasonable modern choice).

### 4.2 Revision tree
Per document, you need a full tree (not just a list) of revisions,
since branches (conflicts) matter:
```
Doc "task:abc123":
  1-aaa
    └─ 2-bbb
         ├─ 3-ccc (winner, e.g. longest/highest-generation, or hash-tiebreak)
         └─ 3-ddd (conflict — still stored, listed in _conflicts)
```
Rust modeling suggestion: a `HashMap<RevId, RevNode>` per document,
where each `RevNode` has `parent: Option<RevId>`, `deleted: bool`, and
either an inline doc body or a pointer to it. Winner computation is a
tree walk: find all leaf nodes (revisions with no children), pick by
generation depth then hash tiebreak.

### 4.3 Sequence numbers / `_changes` ordering
CouchDB uses an internal monotonically-increasing "update sequence" per
database — every write bumps it. `_changes?since=N` must return, in
order, every document whose sequence is `> N`. A simple approach: an
append-only log (Vec/on-disk log) of `(seq, doc_id)` pairs, with a
separate map from `doc_id -> latest revision tree`. Sequence tokens
handed to clients can be opaque strings (CouchDB itself does this in
modern versions, e.g. `"5-g1AAAAFReJz..."`), which gives you room to
change the internal representation later without breaking clients that
just echo the token back.

### 4.4 Winner-picking algorithm (must match client expectations)
1. Find all leaf revisions (no children) that are not `deleted`.
2. If exactly one non-deleted leaf, it wins.
3. Otherwise (real conflict, or all leaves deleted): pick the leaf with
   the **highest generation number**; tie-break by **comparing revision
   hash strings lexicographically, highest wins** (this exact tiebreak
   rule is what real CouchDB uses — verify against a real CouchDB
   instance's behavior in a differential test, see §6.2, if the two
   implementations ever need to agree byte-for-byte).
4. All non-winning, non-deleted leaves become the `_conflicts` array
   when the doc is fetched with `conflicts=true`.

### 4.5 `_local` (checkpoint) documents
Separate namespace from regular docs — `_local/replication-{hash}` style
ids, no revision history (each `PUT` just overwrites), never appear in
`_changes` or regular replication. Store these in a distinct table/map
from regular documents.

---

## 5. Suggested Rust architecture

This is a starting point, not a mandate — adjust to the new project's
actual constraints (embedded in a desktop app? standalone binary? both?).

- **HTTP framework**: `axum` (async, well-maintained, good for exactly
  this shape of JSON REST API) or `actix-web` if you want more raw
  throughput and don't mind a steeper API. Either is fine; don't spend
  much time on this choice.
- **Storage engine**: `sled` (pure-Rust embedded KV store, simplest to
  integrate, good enough for personal/small-team scale) or `rusqlite`
  (SQLite — more mature, easier to inspect/debug with off-the-shelf
  tools, and gives you transactions for free). Avoid RocksDB unless you
  specifically need its write-heavy performance characteristics — it
  pulls in a C++ build dependency, which cuts against "simple to build
  and vendor" if that's part of the motivation.
- **Revision tree storage**: one KV entry per document holding a
  serialized tree structure (bincode/serde), rather than one row per
  revision — simpler to reason about atomicity (a `_bulk_docs` write
  touching one document's tree is a single KV write, so you get
  per-document atomicity for free from the underlying store's own
  write guarantee).
- **`_changes` continuous feed**: an in-process broadcast channel
  (`tokio::sync::broadcast` or similar) that every write publishes to;
  each open continuous-feed connection subscribes and streams matching
  events. Longpoll is the same mechanism with a timeout and "return
  after first event" instead of "keep streaming."
- **Auth**: HTTP Basic to start (matches PouchDB's simplest config
  shape: `new PouchDB(url, {auth: {username, password}})`); layer
  `_session` cookie auth later only if a browser-direct (non-WebView)
  use case needs it.

---

## 6. Testing environment (mandatory, not optional)

Given the correctness bar in §1.2, testing strategy matters as much as
the implementation itself.

### 6.1 Unit tests — revision tree logic
Pure Rust tests, no HTTP involved: build revision trees by hand (linear
history, single conflict, resolved conflict, deleted-then-recreated
document, deep branch) and assert winner-picking, `_conflicts` listing,
and `_revs_diff` output match expected values for each shape. This is
the highest-value test suite — it's where subtle bugs actually live, and
it's fast/deterministic to run.

### 6.2 Differential testing against real CouchDB
Stand up a real, disposable CouchDB instance (Docker container is
easiest) alongside your Rust server. Write a test harness that performs
the *same sequence of operations* (create doc, edit on two "devices,"
force a conflict, resolve it, delete, recreate) against both servers via
raw HTTP, and diffs the resulting `_changes` feed / revision trees /
`_conflicts` output. This is the single most valuable test category for
catching "close enough but not identical" bugs before they reach a real
client. Run this as an occasional/manual suite (needs Docker + real
CouchDB), not necessarily on every CI run, but definitely before any
release.

### 6.3 Integration tests — real PouchDB client against your server
Use Node.js + the real `pouchdb-http` adapter (or run the actual
frontend app, if there is one, in a headless browser) against your Rust
server, and exercise real `db.sync()` calls: two independent PouchDB
instances, both synced to your server, diverge, resolve, converge —
verify from the *client's* point of view, since that's the actual
contract you're building. This catches protocol-shape bugs (wrong JSON
field names, wrong HTTP status codes, wrong `Content-Type`) that
differential testing against raw HTTP might miss because your differential
test's HTTP client is more lenient than PouchDB's own parsing.

### 6.4 Property-based / fuzz testing
Use `proptest` or `quickcheck` to generate random sequences of
create/edit/delete/conflict-inducing operations and assert invariants
that must always hold regardless of the exact sequence: every document
has exactly one winner or is fully deleted; `_changes` never omits a
write; replaying the same `_bulk_docs` batch twice is idempotent;
`_revs_diff` never claims to need a revision the server already has.

### 6.5 Load/soak testing
`_changes?feed=continuous` with many concurrent long-lived connections,
and large `_bulk_docs` batches (hundreds to thousands of docs at once,
matching an initial full-database sync on first pairing) — verify memory
doesn't grow unbounded per connection and large batches complete in
reasonable time. This matters specifically for the "one device does a
fresh initial sync against months of history" case, which is the
heaviest real workload this kind of server sees.

### 6.6 CI structure suggestion
- Fast tier (every commit): unit tests (§6.1) + a small integration
  suite (§6.3) against an in-process server instance, no Docker needed.
- Slow tier (pre-release / nightly): differential tests against real
  CouchDB (§6.2, needs Docker) + fuzz/property tests run for a longer
  budget (§6.4) + a load test pass (§6.5).

---

## 7. Security & production-hardening checklist (decide explicitly, don't inherit defaults)

- **Auth**: never ship "admin party" (no auth) as a default — CouchDB
  itself does this out of the box and it's a well-known footgun.
- **TLS**: decide deliberately whether plaintext HTTP is acceptable for
  your deployment model (home-LAN-only vs. internet-reachable). Don't
  silently inherit "plaintext is fine" from a different project's
  reasoning — it depends entirely on your own threat model.
- **CORS**: only enable it as broadly as your actual client needs (a
  WebView/browser origin) — `origins=*` is convenient but is a real
  widening of the attack surface if the server is ever reachable beyond
  a trusted LAN.
- **Rate limiting / abuse**: real CouchDB has no built-in protection
  against a client hammering `_bulk_docs` — decide if you need any
  before exposing this beyond a fully trusted network.
- **Credential storage**: random per-install generation (not a shared
  default password), stored outside version control, is the safe
  baseline — same principle regardless of language/framework.

---

## 8. Suggested phased plan

1. **Phase 0 — spike**: implement `PUT /{db}`, `GET /{db}`,
   `_bulk_docs` (no conflicts yet, single-writer), `GET /{db}/{id}`.
   Prove a trivial single-direction one-shot replication works against a
   real PouchDB client. No revision trees yet — just last-write-wins.
2. **Phase 1 — real revision trees**: add proper multi-revision
   tracking, `_revs_diff`, `_bulk_get`, winner-picking, `_conflicts`.
   This is where §4 and §6.1/§6.2 matter most.
3. **Phase 2 — live replication**: `_changes` with longpoll and
   continuous feeds, `_local` checkpoint docs, verify `live:true`
   actually works end-to-end including resuming after a restart.
4. **Phase 3 — hardening**: auth, CORS decisions from §7, load testing
   from §6.5, differential testing from §6.2 as a standing pre-release
   gate.
5. **Phase 4 (only if actually needed)** — attachments, Mango/`_find`
   proxying, `_session` cookie auth, anything else a real concrete use
   case demands. Don't build these speculatively.

---

## 9. References

- CouchDB's own replication protocol specification (the closest thing to
  a formal spec — search "CouchDB Replication Protocol" in CouchDB's
  official docs; it documents the exact `_revs_diff`/`_bulk_docs`/
  `_changes` exchange sequence with wire-format examples).
- PouchDB's source (`packages/node_modules/pouchdb-replication` and
  `pouchdb-adapter-http` on GitHub) is the actual ground truth for "what
  does a real client send and expect back" — reading the adapter code
  directly resolves ambiguity in the prose spec faster than guessing.
- `pouchdb-server` (Node.js, npm) is a working reference implementation
  of a PouchDB-compatible server and a good source of "here's every edge
  case we had to handle" if its issue tracker/test suite is browsable.
- Apache CouchDB's own test suite (in its GitHub repo) is a large,
  battle-tested set of behavioral test cases for the exact semantics in
  §4 — even if you don't reuse the test runner, reading the test
  descriptions is a fast way to discover edge cases you haven't thought
  of yet.
