# Production deployment

Decisions here must be made explicitly per plan §7 — see
[doc/open-questions.md](../doc/open-questions.md) for the ones still open
(TLS, CORS, rate limiting).

## Build & run

```bash
docker compose -f prod/docker-compose.yml up -d --build
```

## Auth

HTTP Basic auth is required on every route except `GET /`. Credentials
are random per-install: on first run, the server generates a username
(`admin`) and password, writes them to `<data dir>/credentials.json`,
and logs the username (not the password — read it from the file). Pin
credentials explicitly instead via `COUCHDB_CLONE_USER`/
`COUCHDB_CLONE_PASSWORD` env vars, which always take priority over the
file. Either way, keep `credentials.json` out of version control (it's
already in `.gitignore`) and treat it like any other secret.

## Not yet handled here (fill in before real deployment)
- TLS termination (reverse proxy, e.g. Caddy/nginx, or native TLS in-app)
  — HTTP Basic auth over plaintext HTTP sends credentials in the clear
  on every request; fine on a trusted LAN, not for anything
  internet-reachable.
- CORS is not configured at all yet — see doc/open-questions.md.
- Rate limiting / abuse protection on `_bulk_docs` — none yet.
- Backup strategy for the sled data directory.
