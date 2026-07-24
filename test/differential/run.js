// Differential test (plan §6.2): run the identical operation sequence
// against this Rust server and a real CouchDB, then diff the results.
// This is the single most valuable test category for "close enough but
// not identical" bugs a client would eventually notice the hard way.
//
// Every write here uses new_edits:false with explicit _rev/_revisions we
// control, on both servers identically — that isolates the comparison
// to "does conflict/winner-picking logic agree with real CouchDB",
// rather than getting tripped up by the two servers computing different
// revision hashes for the same content (ours is SHA-256-based, not
// CouchDB's MD5, and the plan never requires byte-for-byte parity there).
//
// Requires:
//   OUR_URL       - this server, default http://127.0.0.1:8085
//   OUR_USER/OUR_PASSWORD       - default testuser/testpass
//   COUCH_URL     - real CouchDB, default http://127.0.0.1:5984
//   COUCH_USER/COUCH_PASSWORD   - required, no default (must be your own admin creds)

const OUR_URL = process.env.OUR_URL || "http://127.0.0.1:8085";
const OUR_USER = process.env.OUR_USER || "testuser";
const OUR_PASSWORD = process.env.OUR_PASSWORD || "testpass";
const COUCH_URL = process.env.COUCH_URL || "http://127.0.0.1:5984";
const COUCH_USER = process.env.COUCH_USER;
const COUCH_PASSWORD = process.env.COUCH_PASSWORD;

const DB_NAME = "difftest_" + Date.now();

function fail(msg) {
  console.error("FAIL:", msg);
  process.exit(1);
}

if (!COUCH_USER || !COUCH_PASSWORD) {
  fail("set COUCH_USER/COUCH_PASSWORD to your real CouchDB's admin credentials");
}

function authHeader(user, pass) {
  return "Basic " + Buffer.from(`${user}:${pass}`).toString("base64");
}

class Server {
  constructor(name, baseUrl, user, password) {
    this.name = name;
    this.baseUrl = baseUrl;
    this.auth = authHeader(user, password);
  }

  async req(method, path, body) {
    const resp = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers: { "Content-Type": "application/json", Authorization: this.auth },
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
    const text = await resp.text();
    let json;
    try {
      json = JSON.parse(text);
    } catch {
      throw new Error(`${this.name} ${method} ${path}: non-JSON response (${resp.status}): ${text}`);
    }
    return { status: resp.status, body: json };
  }

  async createDb() {
    const r = await this.req("PUT", `/${DB_NAME}`);
    if (r.status !== 201 && r.status !== 200) {
      throw new Error(`${this.name}: could not create db: ${r.status} ${JSON.stringify(r.body)}`);
    }
  }

  async deleteDb() {
    await this.req("DELETE", `/${DB_NAME}`);
  }

  async pushRevision(id, rev, parentIds, deleted, body) {
    const [gen, hash] = rev.split(/-(.+)/);
    const ids = [hash, ...parentIds.map((r) => r.split(/-(.+)/)[1])];
    const doc = {
      _id: id,
      _rev: rev,
      _revisions: { start: Number(gen), ids },
      ...(deleted ? { _deleted: true } : {}),
      ...body,
    };
    const r = await this.req("POST", `/${DB_NAME}/_bulk_docs`, { new_edits: false, docs: [doc] });
    if (r.status !== 201 && r.status !== 200) {
      throw new Error(`${this.name}: push ${rev} failed: ${r.status} ${JSON.stringify(r.body)}`);
    }
    if (Array.isArray(r.body) && r.body.some((e) => e.error)) {
      throw new Error(`${this.name}: push ${rev} reported errors: ${JSON.stringify(r.body)}`);
    }
  }

  async getDocWithConflicts(id) {
    const r = await this.req("GET", `/${DB_NAME}/${id}?conflicts=true`);
    return r.body;
  }

  async changesNormalized() {
    const r = await this.req("GET", `/${DB_NAME}/_changes?style=all_docs`);
    // Normalize away server-specific sequence token formats (ours are
    // plain integers, real CouchDB's are opaque strings) - only the
    // logical content should match.
    return r.body.results
      .map((row) => ({
        id: row.id,
        deleted: !!row.deleted,
        revs: row.changes.map((c) => c.rev).sort(),
      }))
      .sort((a, b) => a.id.localeCompare(b.id));
  }

  async revsDiff(id, revs) {
    const r = await this.req("POST", `/${DB_NAME}/_revs_diff`, { [id]: revs });
    return (r.body[id]?.missing || []).slice().sort();
  }
}

function deepEqual(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

async function main() {
  const ours = new Server("ours", OUR_URL, OUR_USER, OUR_PASSWORD);
  const couch = new Server("couchdb", COUCH_URL, COUCH_USER, COUCH_PASSWORD);

  await ours.createDb();
  await couch.createDb();

  // Build the same revision tree on both servers via explicit
  // new_edits:false pushes (plan §4.2's example shape, extended with a
  // deletion + recreation):
  //   1-aaa
  //     └─ 2-bbb
  //          ├─ 2-ccc (conflict, stays a leaf)
  //          └─ 3-ddd
  //               └─ 4-tomb (deleted)
  //                    └─ 5-recreated (undeleted, current winner)
  for (const server of [ours, couch]) {
    await server.pushRevision("doc1", "1-aaa", [], false, { v: 1 });
    await server.pushRevision("doc1", "2-bbb", ["1-aaa"], false, { v: 2 });
    await server.pushRevision("doc1", "2-ccc", ["1-aaa"], false, { v: 3 });
    await server.pushRevision("doc1", "3-ddd", ["2-bbb", "1-aaa"], false, { v: 4 });
    await server.pushRevision("doc1", "4-tomb", ["3-ddd", "2-bbb", "1-aaa"], true, {});
    await server.pushRevision("doc1", "5-recreated", ["4-tomb", "3-ddd", "2-bbb", "1-aaa"], false, { v: 5 });
  }

  let failures = 0;

  const [ourDoc, couchDoc] = await Promise.all([
    ours.getDocWithConflicts("doc1"),
    couch.getDocWithConflicts("doc1"),
  ]);
  console.log(`ours:    _rev=${ourDoc._rev}  _conflicts=${JSON.stringify(ourDoc._conflicts || [])}  deleted=${!!ourDoc._deleted}`);
  console.log(`couchdb: _rev=${couchDoc._rev}  _conflicts=${JSON.stringify(couchDoc._conflicts || [])}  deleted=${!!couchDoc._deleted}`);

  if (ourDoc._rev !== couchDoc._rev) {
    console.error(`MISMATCH: winning rev differs (ours=${ourDoc._rev}, couchdb=${couchDoc._rev})`);
    failures++;
  }
  const ourConflicts = (ourDoc._conflicts || []).slice().sort();
  const couchConflicts = (couchDoc._conflicts || []).slice().sort();
  if (!deepEqual(ourConflicts, couchConflicts)) {
    console.error(`MISMATCH: _conflicts differ (ours=${JSON.stringify(ourConflicts)}, couchdb=${JSON.stringify(couchConflicts)})`);
    failures++;
  }

  const [ourChanges, couchChanges] = await Promise.all([ours.changesNormalized(), couch.changesNormalized()]);
  console.log(`ours _changes:    ${JSON.stringify(ourChanges)}`);
  console.log(`couchdb _changes: ${JSON.stringify(couchChanges)}`);
  if (!deepEqual(ourChanges, couchChanges)) {
    console.error(`MISMATCH: _changes differ`);
    failures++;
  }

  const revsDiffRequest = ["1-aaa", "2-bbb", "99-nonexistent"];
  const [ourMissing, couchMissing] = await Promise.all([
    ours.revsDiff("doc1", revsDiffRequest),
    couch.revsDiff("doc1", revsDiffRequest),
  ]);
  console.log(`ours _revs_diff missing:    ${JSON.stringify(ourMissing)}`);
  console.log(`couchdb _revs_diff missing: ${JSON.stringify(couchMissing)}`);
  if (!deepEqual(ourMissing, couchMissing)) {
    console.error(`MISMATCH: _revs_diff missing list differs`);
    failures++;
  }

  await Promise.all([ours.deleteDb(), couch.deleteDb()]);

  if (failures > 0) {
    fail(`${failures} mismatch(es) found between our server and real CouchDB — see above`);
  }

  console.log("PASS: winner, _conflicts, _changes, and _revs_diff all match real CouchDB");
}

main().catch((err) => fail(err.stack || err));
