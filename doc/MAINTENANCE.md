# Maintenance

## Repo layout
- `doc/` — start with [USAGE.md](USAGE.md).
- `db/` — the Rust server.
- `db/src/routes/` — one module per area of the HTTP API.
- `test/` — HTTP-level suites, `run_all.js` (the full regression), and
  `lib/harness.js` (shared helpers). Unit tests live in `db/src`.
- `prod/` — Dockerfile and deployment config.

## Day-to-day
- Update `USAGE.md` when a route, env var, or behavior changes.
- Update `changelog.md` for any user-visible or protocol change.
- Update `roadmap.md` as phases complete.
- Log deferred decisions in `open-questions.md` instead of silently
  picking a default.

## Before a release
1. `node test/run_all.js` — build with zero warnings, clippy, unit
   tests, every suite, PouchDB 8 and 9. Must exit 0.
2. Needs a real CouchDB: `test/differential/` and
   `test/benchmark/vs_couchdb.js` (update `BENCHMARKS.md`).
3. Re-run the `AUDIT.md` checklist — don't skip this just because it
   passed once.
4. Check `open-questions.md` for anything newly relevant.
5. Update `changelog.md`.
6. Push, wait for CI to pass on the commit, then tag it (`vX.Y.Z`,
   annotated). Add the GitHub release notes as
   `doc/releases/vX.Y.Z.md`: the Release workflow publishes a release
   for any version with both a tag and notes, whichever is pushed last.

## Dependencies
Keep the list small — the whole premise is a small, auditable protocol
surface. Justify any new crate against that.
