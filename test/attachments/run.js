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

const md5Digest = (s) => "md5-" + require("node:crypto").createHash("md5").update(s).digest("base64");

// Stubs must carry CouchDB-style `md5-<base64>` digests (see
// doc/changelog.md, v0.1.7): that's what PouchDB computes for its own
// copies, and what its replicator compares to decide whether it already
// has an attachment.
async function testMd5Digests() {
  const db = "att_md5_" + Date.now();
  await req(db, "PUT", "");
  const json = { "Content-Type": "application/json" };
  await req(
    db,
    "PUT",
    "/doc",
    JSON.stringify({ _attachments: { "f.txt": { content_type: "text/plain", data: Buffer.from("hello md5").toString("base64") } } }),
    json
  );
  const want = md5Digest("hello md5");
  const viaGet = (await req(db, "GET", "/doc")).body._attachments?.["f.txt"]?.digest;
  const viaBulk = (await req(db, "POST", "/_bulk_get", JSON.stringify({ docs: [{ id: "doc" }] }), json)).body.results?.[0]
    ?.docs?.[0]?.ok?._attachments?.["f.txt"]?.digest;
  const viaOpenRevs = (await req(db, "GET", "/doc?open_revs=all")).body?.[0]?.ok?._attachments?.["f.txt"]?.digest;
  check(
    "attachment stubs carry CouchDB-style md5-<base64> digests on every read path",
    viaGet === want && viaBulk === want && viaOpenRevs === want,
    `want ${want}; GET=${viaGet} _bulk_get=${viaBulk} open_revs=${viaOpenRevs}`
  );
  await req(db, "DELETE", "");
}

// The practical effect: a pull of an edited doc whose attachment didn't
// change must not download the attachment again.
async function testPullSkipsUnchangedAttachment() {
  const db = "att_redownload_" + Date.now();
  await req(db, "PUT", "");
  const json = { "Content-Type": "application/json" };
  const created = await req(
    db,
    "PUT",
    "/doc",
    JSON.stringify({ v: 1, _attachments: { "big.bin": { content_type: "application/octet-stream", data: Buffer.alloc(4096, 7).toString("base64") } } }),
    json
  );

  let attachmentGets = 0;
  const remote = new PouchDB(`${SERVER_URL}/${db}`, {
    auth: { username: OUR_USER, password: OUR_PASSWORD },
    fetch: (url, opts) => {
      if (new URL(url).pathname.endsWith("/doc/big.bin")) attachmentGets++;
      return PouchDB.fetch(url, opts);
    },
  });
  const local = new PouchDB("att_redownload_local_" + Date.now());
  try {
    await PouchDB.replicate(remote, local);
    // Edit only the body; keep the attachment as the stub the server gave us.
    const current = (await req(db, "GET", "/doc")).body;
    await req(db, "PUT", "/doc", JSON.stringify({ ...current, _rev: created.body.rev, v: 2 }), json);
    attachmentGets = 0;
    await PouchDB.replicate(remote, local);
    const pulled = await local.get("doc");
    check(
      "pulling an edit that didn't touch the attachment doesn't download it again",
      pulled.v === 2 && attachmentGets === 0,
      `v=${pulled.v}, attachment GETs on second pull: ${attachmentGets}`
    );
  } finally {
    await local.destroy();
    await req(db, "DELETE", "");
  }
}

// A stub claims "the server already has these bytes". If it doesn't,
// storing the stub anyway leaves a doc whose attachment can never be
// read; CouchDB rejects it with 412 missing_stub.
async function testMissingStub() {
  const db = "att_stub_" + Date.now();
  await req(db, "PUT", "");
  const json = { "Content-Type": "application/json" };
  const unknown = await req(
    db,
    "PUT",
    "/doc?new_edits=false",
    JSON.stringify({
      _id: "doc",
      _rev: "1-aaaa",
      _attachments: { "x.txt": { stub: true, content_type: "text/plain", digest: md5Digest("never uploaded"), length: 14 } },
    }),
    json
  );
  check(
    "a stub with a digest the server doesn't have is rejected 412 missing_stub",
    unknown.status === 412 && unknown.body.error === "missing_stub",
    `${unknown.status} ${JSON.stringify(unknown.body)}`
  );

  // A plain PUT may send a bare {stub:true} for an attachment it isn't
  // changing; the server keeps the previous revision's attachment.
  const first = await req(
    db,
    "PUT",
    "/kept",
    JSON.stringify({ _attachments: { "k.txt": { content_type: "text/plain", data: Buffer.from("keep me").toString("base64") } } }),
    json
  );
  const second = await req(
    db,
    "PUT",
    "/kept",
    JSON.stringify({ _rev: first.body.rev, note: "edited", _attachments: { "k.txt": { stub: true } } }),
    json
  );
  const bytes = await req(db, "GET", "/kept/k.txt");
  check(
    "a bare {stub:true} on PUT keeps the previous revision's attachment",
    second.status === 200 && bytes.status === 200 && bytes.body === "keep me",
    `PUT ${second.status} ${JSON.stringify(second.body)}; GET ${bytes.status} ${JSON.stringify(bytes.body)}`
  );
  await req(db, "DELETE", "");
}

// Replaced attachments used to stay on disk forever. POST /{db}/_compact
// deletes bytes no revision refers to; current and conflicting-branch
// attachments must survive it.
async function testCompactCollectsReplacedAttachments() {
  const db = "att_gc_" + Date.now();
  await req(db, "PUT", "");
  const json = { "Content-Type": "application/json" };
  const att = (text) => ({ "f.txt": { content_type: "text/plain", data: Buffer.from(text).toString("base64") } });
  const first = await req(db, "PUT", "/doc", JSON.stringify({ _attachments: att("old bytes") }), json);
  const edited = await req(db, "PUT", "/doc", JSON.stringify({ _rev: first.body.rev, _attachments: att("new bytes") }), json);
  const branchRev = "2-" + "b".repeat(32);
  await req(
    db,
    "PUT",
    "/doc?new_edits=false",
    JSON.stringify({ _id: "doc", _rev: branchRev, _revisions: { start: 2, ids: ["b".repeat(32), first.body.rev.split("-")[1]] }, _attachments: att("branch bytes") }),
    json
  );

  const compacted = await req(db, "POST", "/_compact", JSON.stringify({}), json);
  const current = await req(db, "GET", `/doc/f.txt?rev=${edited.body.rev}`);
  const branch = await req(db, "GET", `/doc/f.txt?rev=${branchRev}`);
  const stale = await req(
    db,
    "PUT",
    "/other",
    JSON.stringify({ _attachments: { "f.txt": { stub: true, content_type: "text/plain", digest: md5Digest("old bytes"), length: 9 } } }),
    json
  );
  check(
    "_compact deletes replaced attachment bytes and keeps current and conflict-branch ones",
    compacted.status === 202 && current.body === "new bytes" && branch.body === "branch bytes" && stale.status === 412,
    `compact ${compacted.status}; current ${JSON.stringify(current.body)}; branch ${JSON.stringify(branch.body)}; old-digest stub ${stale.status}`
  );
  await req(db, "DELETE", "");
}

async function main() {
  await testCompactCollectsReplacedAttachments();
  await testMd5Digests();
  await testPullSkipsUnchangedAttachment();
  await testMissingStub();
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
