// Tests for Phase 4 (attachments): both our own HTTP-level endpoints
// and a real PouchDB client's putAttachment()/getAttachment() methods,
// since that's the actual client this server needs to support.
//
// Requires: SERVER_URL/OUR_USER/OUR_PASSWORD (defaults: 127.0.0.1:8085, testuser/testpass)

const PouchDB = require("pouchdb");

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

async function req(dbName, method, path, body, extraHeaders) {
  const resp = await fetch(`${SERVER_URL}/${dbName}${path}`, {
    method,
    headers: { Authorization: AUTH, ...extraHeaders },
    body,
  });
  const text = await resp.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    json = text;
  }
  return { status: resp.status, body: json, headers: resp.headers };
}

async function testInlineAttachment() {
  const db = "att_inline_" + Date.now();
  await req(db, "PUT", "");

  const data = Buffer.from("hello world").toString("base64");
  await req(
    db,
    "PUT",
    "/doc1",
    JSON.stringify({ title: "test", _attachments: { "hello.txt": { content_type: "text/plain", data } } }),
    { "Content-Type": "application/json" }
  );

  const stubView = await req(db, "GET", "/doc1");
  check(
    "GET without attachments=true returns a stub, not data",
    stubView.body._attachments?.["hello.txt"]?.stub === true && !stubView.body._attachments["hello.txt"].data,
    JSON.stringify(stubView.body)
  );

  const fullView = await req(db, "GET", "/doc1?attachments=true");
  check(
    "GET with attachments=true inlines the original data",
    fullView.body._attachments?.["hello.txt"]?.data === data,
    JSON.stringify(fullView.body)
  );

  const raw = await req(db, "GET", "/doc1/hello.txt");
  check("standalone GET returns raw bytes matching the original", raw.body === "hello world", JSON.stringify(raw.body));
  check(
    "standalone GET sets the stored Content-Type",
    raw.headers.get("content-type") === "text/plain",
    raw.headers.get("content-type")
  );

  await req(db, "DELETE", "");
}

async function testStandaloneUploadAndDelete() {
  const db = "att_standalone_" + Date.now();
  await req(db, "PUT", "");
  await req(db, "PUT", "/doc1", JSON.stringify({ title: "test" }), { "Content-Type": "application/json" });

  await req(db, "PUT", "/doc1/second.txt", "second file content", { "Content-Type": "text/plain" });
  const afterUpload = await req(db, "GET", "/doc1");
  check(
    "standalone PUT adds an attachment without disturbing existing fields",
    afterUpload.body.title === "test" && afterUpload.body._attachments?.["second.txt"]?.length === 19,
    JSON.stringify(afterUpload.body)
  );

  await req(db, "DELETE", "/doc1/second.txt");
  const afterDelete = await req(db, "GET", "/doc1");
  check(
    "standalone DELETE removes just that attachment",
    !afterDelete.body._attachments || !afterDelete.body._attachments["second.txt"],
    JSON.stringify(afterDelete.body)
  );

  await req(db, "DELETE", "");
}

async function testMalformedAttachmentRejected() {
  const db = "att_bad_" + Date.now();
  await req(db, "PUT", "");

  const r = await req(
    db,
    "PUT",
    "/doc1",
    JSON.stringify({ _attachments: { "bad.txt": { content_type: "text/plain", data: "not valid base64!!" } } }),
    { "Content-Type": "application/json" }
  );
  check("malformed base64 attachment is rejected with 400", r.status === 400, JSON.stringify(r.body));

  await req(db, "DELETE", "");
}

// The actual real-world case: a real PouchDB client using its own
// attachment API, not our raw HTTP endpoints directly.
async function testRealPouchDBClient() {
  const dbName = "att_pouchdb_" + Date.now();
  const createResp = await fetch(`${SERVER_URL}/${dbName}`, { method: "PUT", headers: { Authorization: AUTH } });
  if (!createResp.ok) throw new Error(`could not create db: ${createResp.status}`);

  const remote = new PouchDB(`${SERVER_URL}/${dbName}`, { auth: { username: OUR_USER, password: OUR_PASSWORD } });

  await remote.put({ _id: "doc1", title: "has an attachment" });
  const doc = await remote.get("doc1");
  const blob = Buffer.from("attached via real PouchDB client");
  await remote.putAttachment("doc1", "note.txt", doc._rev, blob, "text/plain");

  const fetched = await remote.getAttachment("doc1", "note.txt");
  const fetchedText = Buffer.isBuffer(fetched) ? fetched.toString() : Buffer.from(await fetched.arrayBuffer()).toString();
  check(
    "real PouchDB client: getAttachment returns what putAttachment wrote",
    fetchedText === blob.toString(),
    fetchedText
  );

  const docWithMeta = await remote.get("doc1", { attachments: false });
  check(
    "real PouchDB client: doc metadata lists the attachment as a stub",
    docWithMeta._attachments?.["note.txt"]?.content_type === "text/plain",
    JSON.stringify(docWithMeta._attachments)
  );

  await remote.destroy();
}

// Conflicting branches carrying different bytes under the same attachment
// name (see doc/changelog.md, v0.1.7). A pulling PouchDB fetches each
// attachment with `GET /db/doc/att?rev=<that branch's rev>`; if `rev` is
// ignored, the losing branch either 404s (and the batch retries forever)
// or silently gets the winner's bytes.
async function testConflictingBranchAttachments() {
  const db = "att_conflict_" + Date.now();
  await req(db, "PUT", "");
  const b64 = (s) => Buffer.from(s).toString("base64");
  const push = (h, text) =>
    req(
      db,
      "PUT",
      "/doc?new_edits=false",
      JSON.stringify({
        _id: "doc",
        _rev: `2-${h}`,
        _revisions: { start: 2, ids: [h, "root"] },
        branch: h,
        _attachments: { "f.txt": { content_type: "text/plain", data: b64(text) } },
      }),
      { "Content-Type": "application/json" }
    );
  await req(db, "PUT", "/doc?new_edits=false", JSON.stringify({ _id: "doc", _rev: "1-root", v: 0 }), {
    "Content-Type": "application/json",
  });
  await push("aaaa", "bytes of branch A");
  await push("bbbb", "bytes of branch B"); // winner: same generation, higher hash

  const losingDoc = await req(db, "GET", "/doc?rev=2-aaaa");
  check(
    "GET /db/doc?rev=<losing rev> returns that revision, not the winner",
    losingDoc.status === 200 && losingDoc.body._rev === "2-aaaa" && losingDoc.body.branch === "aaaa",
    JSON.stringify(losingDoc.body)
  );
  const losingAtt = await req(db, "GET", "/doc/f.txt?rev=2-aaaa");
  check(
    "GET /db/doc/att?rev=<losing rev> returns that branch's bytes",
    losingAtt.status === 200 && losingAtt.body === "bytes of branch A",
    `${losingAtt.status} ${JSON.stringify(losingAtt.body)}`
  );
  const winningAtt = await req(db, "GET", "/doc/f.txt?rev=2-bbbb");
  check(
    "GET /db/doc/att?rev=<winning rev> returns the winner's bytes",
    winningAtt.status === 200 && winningAtt.body === "bytes of branch B",
    `${winningAtt.status} ${JSON.stringify(winningAtt.body)}`
  );
  const unknownDoc = await req(db, "GET", "/doc?rev=9-nope");
  const unknownAtt = await req(db, "GET", "/doc/f.txt?rev=9-nope");
  check(
    "an unknown ?rev= is 404 missing on both the doc and the attachment",
    unknownDoc.status === 404 &&
      unknownDoc.body.reason === "missing" &&
      unknownAtt.status === 404 &&
      unknownAtt.body.reason === "missing",
    `${JSON.stringify(unknownDoc.body)} ${JSON.stringify(unknownAtt.body)}`
  );

  // The real case: replicate both branches into a second PouchDB.
  const remote = new PouchDB(`${SERVER_URL}/${db}`, { auth: { username: OUR_USER, password: OUR_PASSWORD } });
  const local = new PouchDB("att_conflict_local_" + Date.now());
  try {
    const replication = PouchDB.replicate(remote, local);
    const timeout = new Promise((_, reject) => setTimeout(() => reject(new Error("replication timed out")), 20000));
    await Promise.race([replication, timeout]);
    const readBranch = async (rev) => {
      const d = await local.get("doc", { rev, attachments: true });
      return Buffer.from(d._attachments["f.txt"].data, "base64").toString();
    };
    const a = await readBranch("2-aaaa");
    const b = await readBranch("2-bbbb");
    check(
      "a replicated PouchDB gets each conflicting branch's own attachment bytes",
      a === "bytes of branch A" && b === "bytes of branch B",
      `A=${JSON.stringify(a)} B=${JSON.stringify(b)}`
    );
  } catch (err) {
    check("a replicated PouchDB gets each conflicting branch's own attachment bytes", false, err.message);
  } finally {
    await local.destroy();
    await req(db, "DELETE", "");
  }
}

async function main() {
  await testInlineAttachment();
  await testStandaloneUploadAndDelete();
  await testMalformedAttachmentRejected();
  await testRealPouchDBClient();
  await testConflictingBranchAttachments();

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nAll attachment tests pass");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
