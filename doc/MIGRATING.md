# Using This Server With an Existing PouchDB App

For an app that already uses PouchDB and currently syncs against a real
CouchDB (or Cloudant, or any CouchDB-compatible server). This covers
pointing the app at this server instead — as a trial, alongside the
existing CouchDB, or as a full switch.

If you haven't run this server at all yet, read [USAGE.md](USAGE.md)
first.

---

## 1. Check compatibility first

Your app almost certainly only uses what's covered here — PouchDB's
`db.sync()`/`db.replicate.to/from()` against a remote is exactly the
protocol surface this server implements. But confirm before switching:

- **Does your app query the remote directly** (not just its local
  PouchDB) using Mango (`db.find()`) or MapReduce views against the
  CouchDB URL? Not supported here. Local queries against the app's own
  local PouchDB database are unaffected either way — those never touch
  the remote server, with any CouchDB.
- **Does your app rely on the remote rejecting a stale `_rev` on a
  plain `PUT`** (optimistic concurrency)? Not enforced here — see
  [USAGE.md §7](USAGE.md#7-differences-from-real-couchdb). This
  basically never matters in practice: real conflict handling happens
  through replication (`new_edits:false`), which *is* fully enforced,
  and that's what `db.sync()` actually uses.
- **Does your app use `multipart/related` for attachment uploads**
  (a PouchDB adapter option, not the default)? Not supported — inline
  base64 attachments are, which is what PouchDB sends by default.
- **Does your app use `_session` cookie auth** instead of HTTP Basic?
  Not supported. If your app currently does `new PouchDB(url, {auth: {username, password}})`
  or embeds credentials in the URL, you're already using HTTP Basic and
  this is a non-issue.

If none of those apply, you're good — this is the common case per
CouchDB's own replication protocol design.

## 2. Run the server

```bash
cargo run --release --manifest-path db/Cargo.toml
```

or via Docker:

```bash
docker compose -f prod/docker-compose.yml up -d --build
```

Note the generated admin username from the startup log, and read the
password from `<data dir>/credentials.json` — or pin both explicitly
with `NYXDB_USER`/`NYXDB_PASSWORD` (recommended for a
real deployment, so you're not hunting for a generated file later).

Confirm it's reachable:

```bash
curl http://<host>:5984/
# {"couchdb":"Welcome",...}
```

## 3. Create the database

Same as any CouchDB — one `PUT` per database your app needs:

```bash
curl -u <user>:<password> -X PUT http://<host>:5984/<your-db-name>
```

## 4. Point the app at it

Wherever your app currently constructs its remote PouchDB instance,
change the URL and credentials:

```js
// Before
const remote = new PouchDB("https://my-couchdb-host:5984/mydb", {
  auth: { username: "admin", password: "..." },
});

// After — same shape, different host/credentials
const remote = new PouchDB("http://<this-server-host>:5984/<your-db-name>", {
  auth: { username: "<user>", password: "<password>" },
});
```

Everything downstream — `db.sync(remote, {live: true, retry: true})`,
`db.replicate.to/from(remote)` — needs no code changes. This is the
entire point of matching the replication protocol.

**If deploying over plaintext HTTP** (the default — see
[open-questions.md](open-questions.md) for why), make sure this is a
trusted network (home LAN, VPN, etc.), not the open internet. HTTP
Basic sends credentials in the clear.

## 5. Move existing data over

If devices already have data synced against the old CouchDB, you don't
need a separate migration step — each device's local PouchDB already
holds a full copy. Just point it at the new remote and let replication
do the work:

```js
await localDb.replicate.to(newRemote);
```

For a one-time bulk copy directly server-to-server (no client involved,
e.g. seeding a fresh deployment from an existing CouchDB's data), use
`test/differential/run.js` as a reference for driving both servers'
raw HTTP APIs, or simplest: spin up a temporary PouchDB instance
pointed at the old CouchDB, then `replicate.to()` the new server.

## 6. Test before fully cutting over

1. Point one device/test build at the new server while others stay on
   the old one.
2. Do a real `db.sync({live: true, retry: true})` and make an edit on
   each side — confirm it converges both ways.
3. Force a conflict on purpose (edit the same doc offline on two
   devices, then let both sync) and confirm it resolves the same way
   it would against the real CouchDB. This is exactly what
   `test/differential/run.js` verifies at the protocol level, so if
   that test passes against your own CouchDB instance, this is already
   covered — but seeing it work with your actual app is worth doing
   once.
4. If your app uses attachments, test a real upload/download round
   trip through the app's own UI, not just the API directly.

## 7. Roll back if needed

Nothing here is destructive to the old CouchDB — switching the remote
URL back is the entire rollback. Keep the old server running until
you're confident, especially for the first real-world test.

## Known gaps to keep in mind

Everything in [USAGE.md §7](USAGE.md#7-differences-from-real-couchdb),
plus:

- No TLS built in — terminate it in front (Caddy/nginx) if this ever
  needs to be internet-reachable.
- No rate limiting — fine for a personal/small-team trusted network,
  not hardened against abuse from an untrusted client.
- Single-node only — no clustering, unlike CouchDB 4.x.

None of these affect a typical local-first app syncing between a
user's own devices on a trusted network, which is what this server was
built for.
