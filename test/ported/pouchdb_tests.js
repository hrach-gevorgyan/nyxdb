// Test cases ported from PouchDB's own integration test suite
// (https://github.com/pouchdb/pouchdb/tree/master/tests/integration),
// adapted to hit this server's HTTP API directly instead of going
// through a PouchDB instance. These are real-world edge cases the
// PouchDB/CouchDB community has already found and fixed bugs for —
// worth checking here rather than trusting our own test cases alone.
//
// Only ported cases that test behavior this server actually implements.
// Several PouchDB tests (revs_limit, auto_compaction, optimistic
// concurrency on a plain PUT, open_revs=all) test features this server
// deliberately doesn't implement yet — see doc/USAGE.md §7. Those are
// not ported; making them pass would mean silently changing documented,
// deliberate behavior.
//
// Requires: SERVER_URL/OUR_USER/OUR_PASSWORD (defaults: 127.0.0.1:8085, testuser/testpass)

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const OUR_USER = process.env.OUR_USER || "testuser";
const OUR_PASSWORD = process.env.OUR_PASSWORD || "testpass";
const AUTH = "Basic " + Buffer.from(`${OUR_USER}:${OUR_PASSWORD}`).toString("base64");

let failures = 0;

function check(name, condition, detail) {
  if (condition) {
    console.log(`PASS: ${name}`);
  } else {
    console.error(`FAIL: ${name}${detail ? " — " + detail : ""}`);
    failures++;
  }
}

async function req(dbName, method, path, body) {
  const resp = await fetch(`${SERVER_URL}/${dbName}${path}`, {
    method,
    headers: { "Content-Type": "application/json", Authorization: AUTH },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await resp.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    json = text;
  }
  return { status: resp.status, body: json };
}

async function createDb(dbName) {
  const r = await req(dbName, "PUT", "");
  if (r.status !== 201 && r.status !== 200) throw new Error(`could not create ${dbName}: ${r.status}`);
}

async function deleteDb(dbName) {
  await req(dbName, "DELETE", "");
}

// PouchDB: "Testing successive new_edits to the same doc" — pushing the
// exact same revision + history twice must be idempotent: no error,
// same resulting rev.
async function testSuccessiveNewEditsIdempotent() {
  const db = "ported_idempotent_" + Date.now();
  await createDb(db);

  const doc = {
    _id: "foobar123",
    _rev: "1-x",
    _revisions: { start: 1, ids: ["x"] },
    integer: 1,
  };

  const first = await req(db, "POST", "/_bulk_docs", { new_edits: false, docs: [doc] });
  const second = await req(db, "POST", "/_bulk_docs", { new_edits: false, docs: [doc] });
  const final = await req(db, "GET", "/foobar123");

  check(
    "successive new_edits:false push of the same doc is idempotent",
    first.status === 200 && second.status === 200 && final.body._rev === "1-x",
    `first=${JSON.stringify(first.body)} second=${JSON.stringify(second.body)} final=${JSON.stringify(final.body)}`
  );

  await deleteDb(db);
}

// PouchDB: "Deletion with new_edits=false" — a tombstone pushed with
// full _revisions history is accepted and the doc reports deleted.
async function testDeletionWithFullHistory() {
  const db = "ported_deletion_" + Date.now();
  await createDb(db);

  await req(db, "POST", "/_bulk_docs", {
    new_edits: false,
    docs: [{ _id: "foo", _rev: "1-x", _revisions: { start: 1, ids: ["x"] }, integer: 1 }],
  });
  await req(db, "POST", "/_bulk_docs", {
    new_edits: false,
    docs: [{ _id: "foo", _rev: "2-y", _revisions: { start: 2, ids: ["y", "x"] }, _deleted: true }],
  });

  const final = await req(db, "GET", "/foo");
  check(
    "deletion via new_edits:false with full history reports deleted",
    final.status === 404 && final.body.reason === "deleted",
    JSON.stringify(final.body)
  );

  await deleteDb(db);
}

// PouchDB: "#5886 bulkGet with reserved id" — a doc id that collides
// with a JS Object.prototype method name ("constructor") must work
// like any other string id. Not a JS-specific bug for us (Rust has no
// such prototype-pollution class of bug), but a cheap, real-world
// sanity check that nothing treats doc ids as anything other than
// plain string keys.
async function testReservedIdDocument() {
  const db = "ported_reserved_id_" + Date.now();
  await createDb(db);

  const put = await req(db, "PUT", "/constructor", { integer: 1 });
  check("PUT with doc id 'constructor' succeeds", put.status === 200 || put.status === 201, JSON.stringify(put.body));

  const get = await req(db, "GET", "/constructor");
  check("GET with doc id 'constructor' returns it", get.body._id === "constructor" && get.body.integer === 1, JSON.stringify(get.body));

  const bulkGet = await req(db, "POST", "/_bulk_get", { docs: [{ id: "constructor" }] });
  const ok = bulkGet.body.results?.[0]?.docs?.[0]?.ok;
  check("_bulk_get with doc id 'constructor' returns it", ok && ok._id === "constructor", JSON.stringify(bulkGet.body));

  const revsDiff = await req(db, "POST", "/_revs_diff", { constructor: ["99-nonexistent"] });
  check(
    "_revs_diff with doc id 'constructor' works normally",
    Array.isArray(revsDiff.body.constructor?.missing) && revsDiff.body.constructor.missing.includes("99-nonexistent"),
    JSON.stringify(revsDiff.body)
  );

  await deleteDb(db);
}

// PouchDB: "Revs diff with empty revs" — an empty requested-revisions
// array must be handled without error, reporting nothing missing.
async function testRevsDiffEmptyArray() {
  const db = "ported_empty_revs_" + Date.now();
  await createDb(db);
  await req(db, "PUT", "/foo", { x: 1 });

  const r = await req(db, "POST", "/_revs_diff", { foo: [] });
  check(
    "_revs_diff with an empty revs array reports nothing missing",
    r.status === 200 && (r.body.foo === undefined || r.body.foo.missing.length === 0),
    JSON.stringify(r.body)
  );

  await deleteDb(db);
}

// Real bug found live-testing against a real PouchDB app (see
// doc/changelog.md): a single `PUT /{db}/{id}?new_edits=false` with an
// explicit `_rev` at a generation that already exists silently applied
// as a normal edit instead of forking a real conflict — the handler
// ignored `?new_edits=` entirely. The equivalent operation via
// `_bulk_docs` already worked correctly (covered by the tests above),
// which is what made this a routing bug, not a rev-tree bug. Verified
// against real CouchDB 3.5.2 to match this exact behavior before
// fixing (see the differential test for the general case; this pins
// the single-PUT path specifically, which the differential test
// doesn't exercise).
async function testSinglePutNewEditsFalseCreatesConflict() {
  const db = "ported_put_new_edits_false_" + Date.now();
  await createDb(db);

  const created = await req(db, "PUT", "/doc1", { title: "original" });
  const originalRev = created.body.rev;

  const forked = await req(db, "PUT", "/doc1?new_edits=false", {
    _id: "doc1",
    _rev: "1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    title: "different content",
  });
  check(
    "single PUT ?new_edits=false stores the client-supplied rev verbatim",
    forked.status === 200 && forked.body.rev === "1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    JSON.stringify(forked.body)
  );

  const final = await req(db, "GET", "/doc1?conflicts=true");
  const conflicts = final.body._conflicts || [];
  check(
    "single PUT ?new_edits=false forks a real conflict, not a silent overwrite",
    conflicts.length === 1 && (conflicts[0] === originalRev || conflicts[0] === "1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
    `original=${originalRev} conflicts=${JSON.stringify(conflicts)} winner=${final.body._rev}`
  );

  await deleteDb(db);
}

// Real bug found live-testing against a real PouchDB app (see
// doc/changelog.md): `_bulk_get` returned `{"error":{"reason":"deleted"}}`
// for a revision that genuinely exists in the tree but happens to be a
// tombstone (a losing conflict branch, or a fully-deleted document's
// winner) — instead of the actual document content with `_deleted:true`
// embedded, as an "ok" result. Confirmed directly against real CouchDB
// 3.5.2 that it always returns "ok" with `_deleted:true` for any
// revision that exists, whether requested explicitly or resolved via
// the winner, and never uses this error shape at all. This mismatch
// broke real PouchDB replication: `getDocs()`/`bulkGet()` treats any
// `doc.error` entry as a hard batch failure ("There was a problem
// getting docs"), so a sync touching an unresolved multi-way conflict
// (e.g. two independently-seeded devices' default docs merging on
// first pairing) aborted permanently instead of completing.
async function testBulkGetReturnsDeletedRevAsOk() {
  const db = "ported_bulk_get_deleted_" + Date.now();
  await createDb(db);

  // Case 1: an explicitly-requested losing conflict leaf that's a tombstone.
  await req(db, "PUT", "/doc1", { title: "original" });
  await req(db, "PUT", "/doc1?new_edits=false", {
    _id: "doc1",
    _rev: "1-00000000000000000000000000000000",
    _deleted: true,
  });
  const losingLeaf = await req(db, "POST", "/_bulk_get", {
    docs: [{ id: "doc1", rev: "1-00000000000000000000000000000000" }],
  });
  const losingOk = losingLeaf.body.results?.[0]?.docs?.[0]?.ok;
  check(
    "_bulk_get for an explicitly-requested deleted conflict leaf returns ok with _deleted:true, not an error",
    losingOk && losingOk._deleted === true,
    JSON.stringify(losingLeaf.body)
  );

  // Case 2: no rev specified, and the current winner is itself a tombstone
  // (a fully-deleted document, no live leaves left).
  const created = await req(db, "PUT", "/doc2", { title: "x" });
  await req(db, "PUT", "/doc2?new_edits=false", {
    _id: "doc2",
    _rev: "2-tomb",
    _revisions: { start: 2, ids: ["tomb", created.body.rev.split("-")[1]] },
    _deleted: true,
  });
  const noRev = await req(db, "POST", "/_bulk_get", { docs: [{ id: "doc2" }] });
  const noRevOk = noRev.body.results?.[0]?.docs?.[0]?.ok;
  check(
    "_bulk_get with no rev for a fully-deleted document's winner returns ok with _deleted:true, not an error",
    noRevOk && noRevOk._deleted === true,
    JSON.stringify(noRev.body)
  );

  // A plain GET (not _bulk_get) for a fully-deleted document must still
  // 404 — that's a genuinely different question ("does this exist right
  // now") and real CouchDB does 404 there. Guards against overcorrecting.
  const plainGet = await req(db, "GET", "/doc2");
  check(
    "plain GET still 404s for a fully-deleted document (unlike _bulk_get)",
    plainGet.status === 404 && plainGet.body.reason === "deleted",
    JSON.stringify(plainGet.body)
  );

  await deleteDb(db);
}

async function main() {
  await testSuccessiveNewEditsIdempotent();
  await testDeletionWithFullHistory();
  await testReservedIdDocument();
  await testRevsDiffEmptyArray();
  await testSinglePutNewEditsFalseCreatesConflict();
  await testBulkGetReturnsDeletedRevAsOk();

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nAll ported PouchDB test cases pass");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
