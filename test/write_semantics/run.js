// Write semantics CouchDB clients rely on (see doc/changelog.md, v0.1.7):
//
// - `_deleted:true` on PUT / `_bulk_docs` deletes (it used to be stored
//   as an ordinary field on a live revision);
// - `DELETE /{db}/{id}` exists; with `?rev=<losing leaf>` it deletes that
//   branch, which is how PouchDB's `db.remove(id, losingRev)` resolves a
//   conflict on a remote (the route used to be missing entirely: 405);
// - a write naming `_rev` of a current leaf extends that leaf;
// - `_local` docs have real `0-N` revisions and can be deleted.
//
// Uses PouchDB (test/integration's install) for the conflict-resolution
// case; everything else is plain fetch.
// Requires: SERVER_URL/OUR_USER/OUR_PASSWORD (defaults: 127.0.0.1:8085, testuser/testpass)

const PouchDB = require("../integration/pouchdb");

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const OUR_USER = process.env.OUR_USER || "testuser";
const OUR_PASSWORD = process.env.OUR_PASSWORD || "testpass";
const AUTH = "Basic " + Buffer.from(`${OUR_USER}:${OUR_PASSWORD}`).toString("base64");

let failures = 0;
function check(name, condition, detail) {
  if (condition) console.log(`PASS: ${name}`);
  else {
    console.error(`FAIL: ${name}${detail ? " — " + detail : ""}`);
    failures++;
  }
}

async function req(db, method, path, body) {
  const resp = await fetch(`${SERVER_URL}/${db}${path}`, {
    method,
    headers: { "Content-Type": "application/json", Authorization: AUTH },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  return { status: resp.status, body: await resp.json().catch(() => null) };
}

// d: 1-a -> 2-aaaa (loser) and 1-a -> 2-bbbb (winner)
async function conflicted(db, id) {
  await req(db, "PUT", `/${id}?new_edits=false`, { _id: id, _rev: "1-a", v: 1 });
  await req(db, "PUT", `/${id}?new_edits=false`, { _id: id, _rev: "2-aaaa", _revisions: { start: 2, ids: ["aaaa", "a"] }, v: "loser" });
  await req(db, "PUT", `/${id}?new_edits=false`, { _id: id, _rev: "2-bbbb", _revisions: { start: 2, ids: ["bbbb", "a"] }, v: "winner" });
}

async function main() {
  const db = "writesem_" + Date.now();
  await req(db, "PUT", "");
  try {
    const created = await req(db, "PUT", "/put-del", { v: 1 });
    const putDel = await req(db, "PUT", "/put-del", { _rev: created.body.rev, _deleted: true });
    const afterPutDel = await req(db, "GET", "/put-del");
    check(
      "PUT with _deleted:true deletes the doc",
      putDel.status === 200 && afterPutDel.status === 404 && afterPutDel.body?.reason === "deleted",
      `${putDel.status} ${JSON.stringify(putDel.body)} / ${afterPutDel.status} ${JSON.stringify(afterPutDel.body)}`
    );

    const bulkCreated = await req(db, "PUT", "/bulk-del", { v: 1 });
    await req(db, "POST", "/_bulk_docs", { docs: [{ _id: "bulk-del", _rev: bulkCreated.body.rev, _deleted: true }] });
    const afterBulkDel = await req(db, "GET", "/bulk-del");
    const changes = await req(db, "GET", "/_changes");
    const row = changes.body?.results?.find((r) => r.id === "bulk-del");
    check(
      "_bulk_docs with _deleted:true deletes, and _changes marks it deleted",
      afterBulkDel.status === 404 && afterBulkDel.body?.reason === "deleted" && row?.deleted === true,
      `${afterBulkDel.status} ${JSON.stringify(row)}`
    );

    const plain = await req(db, "PUT", "/plain-del", { v: 1 });
    const del = await req(db, "DELETE", `/plain-del?rev=${plain.body.rev}`);
    const afterDel = await req(db, "GET", "/plain-del");
    check(
      "DELETE /db/doc?rev=<current> deletes it and returns the tombstone rev",
      del.status === 200 && del.body?.ok && /^2-/.test(del.body?.rev) && afterDel.status === 404 && afterDel.body?.reason === "deleted",
      `${del.status} ${JSON.stringify(del.body)} / ${afterDel.status}`
    );

    await conflicted(db, "c1");
    const delLoser = await req(db, "DELETE", "/c1?rev=2-aaaa");
    const c1 = await req(db, "GET", "/c1?conflicts=true");
    check(
      "DELETE ?rev=<losing leaf> removes that branch and leaves the winner",
      delLoser.status === 200 && c1.body?._rev === "2-bbbb" && c1.body?.v === "winner" && !c1.body?._conflicts,
      `${delLoser.status} ${JSON.stringify(delLoser.body)} / ${JSON.stringify(c1.body)}`
    );

    const badRev = await req(db, "DELETE", "/c1?rev=1-a");
    check("DELETE ?rev=<not a current leaf> is 409 conflict", badRev.status === 409 && badRev.body?.error === "conflict", `${badRev.status} ${JSON.stringify(badRev.body)}`);

    await conflicted(db, "c2");
    const extend = await req(db, "PUT", "/c2", { _rev: "2-aaaa", v: "loser, edited" });
    const leaves = (await req(db, "GET", "/c2?open_revs=all")).body.map((e) => e.ok?._rev).sort();
    check(
      "a write naming a current losing leaf's _rev extends that leaf",
      extend.status === 200 && leaves.length === 2 && leaves.includes("2-bbbb") && leaves.some((r) => r.startsWith("3-")),
      `${JSON.stringify(extend.body)} leaves=${JSON.stringify(leaves)}`
    );

    // The real client path: PouchDB resolving a conflict on a remote.
    await conflicted(db, "c3");
    const remote = new PouchDB(`${SERVER_URL}/${db}`, { auth: { username: OUR_USER, password: OUR_PASSWORD } });
    let removeError = null;
    try {
      await remote.remove("c3", "2-aaaa");
    } catch (err) {
      removeError = err;
    }
    const c3 = await req(db, "GET", "/c3?conflicts=true");
    check(
      "PouchDB remote.remove(id, losingRev) resolves the conflict",
      !removeError && c3.body?._rev === "2-bbbb" && !c3.body?._conflicts,
      removeError ? `${removeError.status} ${removeError.message}` : JSON.stringify(c3.body)
    );

    const l1 = await req(db, "PUT", "/_local/ckpt", { last_seq: 1 });
    const l2 = await req(db, "PUT", "/_local/ckpt", { _rev: l1.body?.rev, last_seq: 2 });
    const lget = await req(db, "GET", "/_local/ckpt");
    check(
      "_local writes get increasing 0-N revisions, and GET reports the current one",
      l1.body?.rev === "0-1" && l2.body?.rev === "0-2" && lget.body?._rev === "0-2" && lget.body?.last_seq === 2,
      `${JSON.stringify(l1.body)} ${JSON.stringify(l2.body)} ${JSON.stringify(lget.body)}`
    );
    const ldel = await req(db, "DELETE", "/_local/ckpt");
    const lgone = await req(db, "GET", "/_local/ckpt");
    check("DELETE /_local/{id} removes it", ldel.status === 200 && lgone.status === 404, `${ldel.status} ${lgone.status}`);
  } finally {
    await req(db, "DELETE", "");
  }

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nWrite semantics match what CouchDB clients expect");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
