// Integration test (plan §6.3): drive a real PouchDB client against the
// Rust server. Verifies db.replicate.to() moves docs from a local
// in-memory PouchDB into the remote server, matching Phase 0 scope
// (one-shot, single-direction, no conflicts).
//
// Expects the server running at SERVER_URL (default http://127.0.0.1:8085)
// with COUCHDB_CLONE_USER/COUCHDB_CLONE_PASSWORD matching TEST_USER/TEST_PASSWORD.

const PouchDB = require("pouchdb");

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const TEST_USER = process.env.TEST_USER || "testuser";
const TEST_PASSWORD = process.env.TEST_PASSWORD || "testpass";
const DB_NAME = "inttest_" + Date.now();
const AUTH_HEADER = "Basic " + Buffer.from(`${TEST_USER}:${TEST_PASSWORD}`).toString("base64");

function fail(msg) {
  console.error("FAIL:", msg);
  process.exit(1);
}

async function main() {
  const local = new PouchDB(DB_NAME);
  await local.put({ _id: "doc1", foo: "bar" });
  await local.put({ _id: "doc2", foo: "baz" });

  const remoteUrl = `${SERVER_URL}/${DB_NAME}`;
  // Create the remote database first (server requires explicit PUT /{db}).
  const createResp = await fetch(remoteUrl, { method: "PUT", headers: { Authorization: AUTH_HEADER } });
  if (!createResp.ok) {
    fail(`could not create remote db: ${createResp.status} ${await createResp.text()}`);
  }

  const remote = new PouchDB(remoteUrl, { auth: { username: TEST_USER, password: TEST_PASSWORD } });
  const result = await local.replicate.to(remote);

  if (result.docs_written !== 2) {
    fail(`expected 2 docs written, got ${result.docs_written}`);
  }

  const fetched1 = await fetch(`${remoteUrl}/doc1`, { headers: { Authorization: AUTH_HEADER } }).then((r) => r.json());
  const fetched2 = await fetch(`${remoteUrl}/doc2`, { headers: { Authorization: AUTH_HEADER } }).then((r) => r.json());

  if (fetched1.foo !== "bar") fail(`doc1.foo mismatch: ${JSON.stringify(fetched1)}`);
  if (fetched2.foo !== "baz") fail(`doc2.foo mismatch: ${JSON.stringify(fetched2)}`);

  console.log("PASS: one-shot db.replicate.to() moved", result.docs_written, "docs to the Rust server");
  await local.destroy();
}

main().catch((err) => fail(err.stack || err));
