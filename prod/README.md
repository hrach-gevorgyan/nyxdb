# Production deployment

Decisions here must be made explicitly per plan §7 — see
[doc/open-questions.md](../doc/open-questions.md) for the ones still open
(TLS, CORS, rate limiting, credential generation).

## Build & run

```bash
docker compose -f prod/docker-compose.yml up -d --build
```

## Not yet handled here (fill in before real deployment)
- TLS termination (reverse proxy, e.g. Caddy/nginx, or native TLS in-app).
- Auth: currently no auth is wired into the server at all — do not deploy
  beyond a fully trusted network until HTTP Basic auth (plan §5, §7) is
  implemented.
- Backup strategy for the sled data directory.
