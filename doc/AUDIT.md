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

### Risky — fixed in a follow-up pass, same session
1. **No request body size limit.** A client could send an arbitrarily
   large body, or an unbounded `_bulk_docs` batch, fully buffered into
   memory before parsing. Fixed with `axum::extract::DefaultBodyLimit`,
   defaulting to 50MB (generous for a large sync batch, not unbounded),
   configurable via `COUCHDB_CLONE_MAX_BODY_BYTES`. Verified: an
   oversized body gets `413` with a proper JSON error (via the
   normalize-error-body fix above); normal requests are unaffected.
2. **Panics on corrupted on-disk data.** `storage.rs` used `.expect()`
   deserializing stored documents and sequence-log entries — a
   corrupted data directory would panic the request instead of
   returning a clean 500. Converted every one of these to a proper
   `StorageError`/`StorageResult` chain (`db/src/storage.rs`) instead
   of panicking. Verified: full regression suite (19 unit tests, both
   PouchDB integration tests, differential test, ported tests) still
   passes — this was a pure error-handling refactor, no behavior change
   on the non-corrupted path.

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

See `changelog.md` for the actual commits. Summary: fixed all five
"risky" items above (three immediately, two in a careful follow-up
pass with full regression testing each time). Left the "review" items
as documented, deliberate gaps — not fixed in a rush, not forgotten
either.
