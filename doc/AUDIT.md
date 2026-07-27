# Project Audit

A full pass over the codebase before starting Phase 4 (attachments) —
correctness, stability, and security. Written up so future audits can
follow the same process instead of starting from scratch.

## How this audit was done

1. **Panic surface**: grepped every source file for `.unwrap()`,
   `.expect()`, `panic!`, `unreachable!`, indexing/slicing that could
   panic on unexpected input. Checked each one for whether it's
   reachable from attacker-controlled input (a request) vs. only from
   trusted input (startup config, our own previously-written data).
2. **Dependency vulnerabilities**: `cargo audit` against the full
   dependency tree (needs `cargo install cargo-audit` once).
3. **Live probing**: actually sent malformed/unusual requests to a
   running server and read the real response, rather than assuming
   from the code — this is how the trailing-slash and empty-error-body
   bugs got caught back in Phase 0, and it caught three more instances
   of the same bug class here.
4. **Auth-specific review**: credential comparison method, credential
   file storage/permissions, what's exempted from auth and why.
5. **Resource-exhaustion check**: request body size limits, batch size
   limits, connection limits — anything an attacker could use to
   exhaust memory or file descriptors without needing valid credentials
   (or even with them, from a misbehaving rather than malicious client).
6. **Deployment context**: checked the Dockerfile specifically, since
   findings that don't matter on Windows (file permissions) can matter
   a lot on the Linux deployment path.

Repeat this checklist before any future release, not just before
Phase 4.

## Findings

### Safe — verified, no action needed
- No known CVEs in the dependency tree (`cargo audit`, 205 crates
  scanned). Three "unmaintained" warnings (`bincode`, `fxhash`,
  `instant`) — maintenance-status only, not vulnerabilities.
- No `panic!`/`unreachable!`/`todo!` anywhere in the source.
- No SQL or shell execution anywhere — no injection surface of that kind.
- CORS is correctly scoped: explicit allowlist, no wildcard, credentials
  not enabled.
- Malformed `Authorization` headers and non-object entries in
  `_bulk_docs` are handled gracefully (no panic).
- `Db::open` runs on every request but sled caches trees internally —
  not a performance problem.
- Conflict/winner-picking logic verified against real CouchDB
  (differential test) and PouchDB's own test suite (ported tests).

### Risky — real problems, fixed in this session
1. **Axum's built-in rejections don't return JSON.** Malformed JSON
   body, wrong/missing `Content-Type`, and genuinely unmatched routes
   all returned plain-text or empty bodies instead of this project's
   `{"error":...,"reason":...}` format. Same bug class already fixed
   once for auth/trailing-slash in Phase 0 — three more live-confirmed
   instances found by actually sending bad requests. Breaks any
   JSON-only client (PouchDB included) that hits one of these paths.
2. **Non-constant-time credential comparison.** `auth.rs` compared
   username/password with plain `==`, a timing side-channel. Lower
   real-world severity here since HTTP Basic auth already sends
   credentials in cleartext over plain HTTP, but a one-line fix.
3. **`credentials.json` had no explicit file permissions.** On Linux
   (the Docker deployment path), a freshly-written file inherits the
   process umask — often world-readable. Fixed to `0o600` on Unix.
   Verified by code review only — this dev machine is Windows, where
   the `#[cfg(unix)]` block doesn't compile in, so the fix can't be
   runtime-tested here. The pattern (`std::fs::set_permissions` +
   `PermissionsExt::from_mode`) is standard; worth a quick check on an
   actual Linux box before relying on it.

### Risky — real problems, deferred (need more care than a quick fix)
1. **No request body size limit.** A client can send an arbitrarily
   large body, or an unbounded `_bulk_docs` batch, fully buffered into
   memory before parsing. Real memory-exhaustion vector. Not fixed
   this session because the right limit is a judgment call (default
   too low breaks legitimate large syncs; too high doesn't help) —
   logged in `open-questions.md`.
2. **Panics on corrupted on-disk data.** `storage.rs` uses `.expect()`
   when deserializing stored documents and sequence-log entries. If
   the data directory is ever corrupted (disk failure, manual
   tampering, a future storage-format change without a migration
   path), a read panics per-request instead of failing with a clean
   500. Tokio isolates the panic to that one request rather than
   crashing the server, but it's still an ungracious failure mode.
   Not fixed this session — converting every one of these to a proper
   `Result` chain touches the core read path and deserves its own
   focused pass with full regression testing, not a rushed edit
   alongside everything else here.

### Review — needs a decision, not urgent
- **No rate limiting or lockout on repeated failed auth attempts.**
  Already an open question; still open.
- **No cap on concurrent `feed=continuous` connections.** Verified
  correct up to 80 concurrent in load testing, but not verified against
  resource exhaustion at much larger scale from a hostile client.
- **`Credentials` derives `Debug`.** Not logged anywhere today, but a
  future `{:?}` on `AppState` or `Credentials` would leak the password
  into logs. Cheap to harden even though nothing exploits it currently.
- **Docker image runs as root** — no `USER` directive. Standard
  hardening practice, low urgency for a personal/small-team LAN
  deployment.
- **Three unmaintained dependencies** (`bincode`, `fxhash`, `instant`).
  No known vulnerabilities today; revisit if a real advisory appears.

## What changed this session

See `changelog.md` for the actual commits. Summary: fixed the three
"risky, fixed" items above. Left the two "risky, deferred" items and
all "review" items as documented, deliberate gaps — not fixed in a
rush, not forgotten either.
