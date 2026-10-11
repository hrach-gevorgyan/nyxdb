// Revision history on the read paths (see doc/changelog.md, v0.1.6):
// `_bulk_get` with `revs`/`latest`, and `GET /{db}/{id}` with `revs`,
// `revs_info`, `open_revs`, `latest`. Response shapes follow CouchDB's
// documented API: `_revisions` is `{start, ids}` with hash parts only,
// newest first; `open_revs` answers with an array of `{ok: doc}` /
// `{missing: rev}`.
//
// No dependencies (native fetch). Builds a known revision tree with
// `PUT ?new_edits=false` so every expected rev id is fixed:
//
//   d:  1-a ── 2-b ── 3-c      (winner)
//         └── 2-x              (live conflict branch)
//   t:  1-a ── 2-t             (tombstone; fully deleted doc)
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
    headers: { "Content-Type": "application/json", Accept: "application/json", Authorization: AUTH },
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

// Key-order-insensitive: JSON object key order isn't part of the API.
const same = (a, b) => require("node:util").isDeepStrictEqual(a, b);
const revsOf = (docs) => docs.map((d) => d.ok?._rev).sort();
const findOk = (docs, rev) => docs.find((d) => d.ok?._rev === rev)?.ok;

async function buildTree(db) {
  const push = (id, rev, ids, extra = {}) =>
    req(db, "PUT", `/${id}?new_edits=false`, { _id: id, _rev: rev, _revisions: { start: ids.length, ids }, ...extra });
  await push("d", "1-a", ["a"], { v: 1 });
  await push("d", "2-b", ["b", "a"], { v: 2 });
  await push("d", "3-c", ["c", "b", "a"], { v: 3 });
  await push("d", "2-x", ["x", "a"], { v: "branch" });
  await push("t", "1-a", ["a"], { v: 1 });
  await push("t", "2-t", ["t", "a"], { _deleted: true });
}

async function testBulkGet(db) {
  const docsFor = (r) => r.body.results?.[0]?.docs || [];

  const q = await req(db, "POST", "/_bulk_get?revs=true", { docs: [{ id: "d", rev: "2-b" }] });
  check(
    "_bulk_get ?revs=true returns _revisions walking back from the requested rev",
    same(docsFor(q)[0]?.ok?._revisions, { start: 2, ids: ["b", "a"] }),
    JSON.stringify(q.body)
  );

  const b = await req(db, "POST", "/_bulk_get", { revs: true, docs: [{ id: "d", rev: "2-b" }] });
  check(
    "_bulk_get with revs:true in the body returns _revisions too",
    same(docsFor(b)[0]?.ok?._revisions, { start: 2, ids: ["b", "a"] }),
    JSON.stringify(b.body)
  );

  const plain = await req(db, "POST", "/_bulk_get", { docs: [{ id: "d", rev: "2-b" }] });
  check(
    "_bulk_get without revs leaves _revisions out",
    docsFor(plain)[0]?.ok && docsFor(plain)[0].ok._revisions === undefined,
    JSON.stringify(plain.body)
  );

  const latest = await req(db, "POST", "/_bulk_get?revs=true&latest=true", { docs: [{ id: "d", rev: "1-a" }] });
  const latestDocs = docsFor(latest);
  check(
    "_bulk_get latest=true on an ancestor returns every leaf descending from it",
    same(revsOf(latestDocs), ["2-x", "3-c"]),
    JSON.stringify(latest.body)
  );
  check(
    "_bulk_get latest=true + revs=true walks back from each returned leaf",
    same(findOk(latestDocs, "3-c")?._revisions, { start: 3, ids: ["c", "b", "a"] }) &&
      same(findOk(latestDocs, "2-x")?._revisions, { start: 2, ids: ["x", "a"] }),
    JSON.stringify(latest.body)
  );

  const latestMid = await req(db, "POST", "/_bulk_get?latest=true", { docs: [{ id: "d", rev: "2-b" }] });
  check(
    "_bulk_get latest=true on 2-b returns only its own descendant leaf 3-c",
    same(revsOf(docsFor(latestMid)), ["3-c"]),
    JSON.stringify(latestMid.body)
  );

  const missing = await req(db, "POST", "/_bulk_get?revs=true&latest=true", { docs: [{ id: "d", rev: "7-zz" }] });
  check(
    "_bulk_get latest=true for an unknown rev still reports missing",
    docsFor(missing)[0]?.error?.reason === "missing",
    JSON.stringify(missing.body)
  );

  // Tombstone handling from v0.1.4 must survive: still "ok" with
  // _deleted:true, now with its history attached.
  const tomb = await req(db, "POST", "/_bulk_get?revs=true", { docs: [{ id: "t", rev: "2-t" }] });
  const tombOk = docsFor(tomb)[0]?.ok;
  check(
    "_bulk_get revs=true on a tombstone is ok with _deleted:true and _revisions",
    tombOk?._deleted === true && same(tombOk?._revisions, { start: 2, ids: ["t", "a"] }),
    JSON.stringify(tomb.body)
  );
}

async function testGetDoc(db) {
  const revs = await req(db, "GET", "/d?revs=true");
  check(
    "GET ?revs=true returns the winner's _revisions",
    revs.body._rev === "3-c" && same(revs.body._revisions, { start: 3, ids: ["c", "b", "a"] }),
    JSON.stringify(revs.body)
  );

  const info = await req(db, "GET", "/d?revs_info=true");
  check(
    "GET ?revs_info=true returns _revs_info along the winner's history",
    same(info.body._revs_info, [
      { rev: "3-c", status: "available" },
      { rev: "2-b", status: "available" },
      { rev: "1-a", status: "available" },
    ]),
    JSON.stringify(info.body)
  );

  const plain = await req(db, "GET", "/d");
  check(
    "GET without flags leaves _revisions/_revs_info out",
    plain.body._revisions === undefined && plain.body._revs_info === undefined,
    JSON.stringify(plain.body)
  );

  const all = await req(db, "GET", "/d?open_revs=all");
  check(
    "GET ?open_revs=all returns an array of {ok: doc}, one per leaf",
    Array.isArray(all.body) && same(revsOf(all.body), ["2-x", "3-c"]),
    JSON.stringify(all.body)
  );

  const list = await req(db, "GET", `/d?open_revs=${encodeURIComponent('["2-b","9-nope"]')}&revs=true`);
  check(
    "GET ?open_revs=[...] returns {ok} for stored revs and {missing} for unknown ones",
    Array.isArray(list.body) &&
      list.body.length === 2 &&
      list.body.some((e) => e.ok?._rev === "2-b" && same(e.ok._revisions, { start: 2, ids: ["b", "a"] })) &&
      list.body.some((e) => e.missing === "9-nope"),
    JSON.stringify(list.body)
  );

  const latest = await req(db, "GET", `/d?open_revs=${encodeURIComponent('["1-a"]')}&latest=true&revs=true`);
  check(
    "GET ?open_revs=[ancestor]&latest=true returns the descendant leaves with history",
    Array.isArray(latest.body) &&
      same(revsOf(latest.body), ["2-x", "3-c"]) &&
      same(findOk(latest.body, "3-c")?._revisions, { start: 3, ids: ["c", "b", "a"] }),
    JSON.stringify(latest.body)
  );

  const tombAll = await req(db, "GET", "/t?open_revs=all&revs=true");
  check(
    "GET ?open_revs=all includes a tombstone leaf as {ok} with _deleted:true",
    Array.isArray(tombAll.body) && tombAll.body[0]?.ok?._deleted === true && tombAll.body[0].ok._rev === "2-t",
    JSON.stringify(tombAll.body)
  );

  const tombPlain = await req(db, "GET", "/t");
  check(
    "plain GET still 404s for a fully-deleted document",
    tombPlain.status === 404 && tombPlain.body.reason === "deleted",
    JSON.stringify(tombPlain.body)
  );
}

async function main() {
  const db = "revisions_" + Date.now();
  const created = await req(db, "PUT", "");
  if (created.status !== 200 && created.status !== 201) throw new Error(`could not create ${db}: ${created.status}`);
  try {
    await buildTree(db);
    await testBulkGet(db);
    await testGetDoc(db);
  } finally {
    await req(db, "DELETE", "");
  }

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nAll revision-history read-path tests pass");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
