# Maintenance

## Repo layout
- `doc/` — start with [USAGE.md](USAGE.md).
- `db/` — the Rust server.
- `test/` — unit, integration, differential, load, and benchmark tests.
- `prod/` — Dockerfile and deployment config.

## Day-to-day
- Update `USAGE.md` when a route, env var, or behavior changes.
- Update `changelog.md` for any user-visible or protocol change.
- Update `roadmap.md` as phases complete.
- Log deferred decisions in `open-questions.md` instead of silently
  picking a default.

## Before a release
1. Fast tier: unit + in-process integration tests.
2. Slow tier: `test/differential/` (needs a real CouchDB instance),
   `test/load/`, `test/benchmark/`, `test/attachments/`.
3. Re-run the `AUDIT.md` checklist — don't skip this just because it
   passed once.
4. Check `open-questions.md` for anything newly relevant.
5. Update `changelog.md`.

## Dependencies
Keep the list small — the whole premise is a small, auditable protocol
surface. Justify any new crate against that.
