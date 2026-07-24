# Maintenance

Also update [doc/USAGE.md](USAGE.md) — the endpoint reference, config
table, benchmark numbers, and CouchDB-difference list — whenever a route,
env var, or behavior changes. It's meant to be the single up-to-date
reference for both users and AI agents; letting it drift defeats the
purpose.

## Repo layout
- `doc/` — planning and process docs (this folder). Start with
  [USAGE.md](USAGE.md) for how to actually use the server.
- `db/` — main codebase: the Rust server implementing the CouchDB
  replication protocol subset (see `rust-couchdb-clone-plan.md` §3).
- `test/` — testing environment: unit/integration tests, differential
  testing harness against real CouchDB (needs Docker), fuzz/property tests.
- `prod/` — production deployment: Dockerfile, prod config, compose files.

## Day-to-day
- Update `doc/changelog.md` for any user-visible or protocol-relevant change.
- Update `doc/roadmap.md` checkboxes as phases complete.
- Log any deferred decision in `doc/open-questions.md` rather than silently
  picking a default — resolve explicitly when the relevant phase starts.

## Before a release
1. Run the fast test tier (unit + in-process integration).
2. Run the slow tier: differential tests against real CouchDB (Docker),
   fuzz/property tests, load test pass.
3. Review `doc/open-questions.md` for anything that must be resolved before
   this release's deployment target (e.g. TLS/CORS decisions for prod).
4. Update `doc/changelog.md`.

## Dependencies
Keep the dependency list intentionally small — this project's whole premise
is a small, auditable protocol surface. Justify any new crate against that.
