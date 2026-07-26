# Production deployment

See [doc/open-questions.md](../doc/open-questions.md) for what's still
undecided (TLS, CORS, rate limiting).

## Build & run

```bash
docker compose -f prod/docker-compose.yml up -d --build
```

## Auth

HTTP Basic auth is required on every route except `GET /`. On first
run, the server generates a username (`admin`) and password, writes
them to `<data dir>/credentials.json`, and logs the username only. Pin
credentials instead via `COUCHDB_CLONE_USER`/`COUCHDB_CLONE_PASSWORD`.
Keep `credentials.json` out of version control (already in
`.gitignore`).

## Not yet handled here (fill in before real deployment)
- TLS termination (reverse proxy, e.g. Caddy/nginx, or native TLS in-app)
  — HTTP Basic auth over plaintext HTTP sends credentials in the clear
  on every request; fine on a trusted LAN, not for anything
  internet-reachable.
- CORS is disabled by default. Set `COUCHDB_CLONE_CORS_ORIGINS` (comma-
  separated, no wildcard) if a browser/WebView client needs it.
- Rate limiting / abuse protection on `_bulk_docs` — none yet.
- Backup strategy for the sled data directory.
