# Security Policy

## Supported versions

This is a young, single-maintainer project. Only the latest tagged
release gets security fixes — there's no backport policy for older
versions yet.

| Version | Supported |
|---|---|
| Latest (`v0.1.x`) | ✅ |
| Older | ❌ |

## Reporting a vulnerability

Please **don't open a public GitHub issue** for a security problem —
report it privately first so there's time to fix it before details
are public.

Preferred: use GitHub's [private vulnerability reporting](../../security/advisories/new)
for this repo (Security tab → "Report a vulnerability").

If that's not workable, email **hrach.gevorgyan@yandex.com** with:
- What the issue is and where (endpoint, file, dependency)
- Steps to reproduce, or a minimal example
- What you think the impact is

You should get a response within a few days. This is a personal
project, not a company with an SLA — please be patient, and thank you
for reporting responsibly instead of disclosing immediately.

## What's in scope

- The Rust server itself (`db/`) — auth, request handling, storage,
  replication-protocol logic.
- The Docker deployment files (`prod/`).
- Dependencies in `Cargo.lock` / the `package-lock.json` files under
  `test/` (even though the latter never ship — see below).

## What's already known and tracked

Known dependency findings are documented rather than hidden:
- [doc/AUDIT.md](doc/AUDIT.md) — the project's own audit process and
  findings, including accepted/deferred items with reasoning.
- [doc/open-questions.md](doc/open-questions.md) — deliberate
  trade-offs (e.g. plaintext HTTP by default, meant for a trusted LAN,
  not the open internet — see that file for why).

`cargo audit` and `npm audit` are run as part of the release process,
not just once. If you find something not already listed in
`doc/AUDIT.md`, it's a real report — please send it in.

## Deployment note

NyxDB defaults to plaintext HTTP and is designed for a trusted network
(home LAN, VPN) — see [doc/open-questions.md](doc/open-questions.md)
for the reasoning and [doc/MIGRATING.md](doc/MIGRATING.md) for
deployment guidance. Running it directly on the open internet without
a TLS-terminating reverse proxy in front is a configuration issue, not
a vulnerability in the code — but if you think there's a real gap in
that guidance itself, that's worth reporting too.
