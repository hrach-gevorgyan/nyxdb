# Changelog

All notable changes to this project are documented here.
Format loosely follows [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]
- Project scaffolding created (doc/, db/, test/, prod/).
- Phase 0 spike: `GET /`, `PUT/GET /{db}`, `GET/PUT /{db}/{id}` implemented
  and manually verified against a running server.
- Fixed: `PUT /{db}` on an existing database returned 200 instead of 412
  (existence check compared against the wrong sled tree name).
- Fixed: server panicked on every `GET`/second `PUT` because `RevTree` was
  serialized with `bincode`, which can't deserialize the embedded
  `serde_json::Value` doc bodies. Switched storage encoding to `serde_json`.
- Added `POST /{db}/_bulk_docs` (batch write, last-write-wins per doc,
  matches `put_doc`'s generation logic). Does not yet honor
  `new_edits:false` — deferred to Phase 1 where real revision trees land.
