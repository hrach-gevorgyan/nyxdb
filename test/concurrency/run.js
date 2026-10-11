// Concurrent writes to the same document must never lose an acknowledged
// revision (see doc/changelog.md, v0.1.7).
//
// Every write is read tree → modify → write tree. Without a lock around
// that, two simultaneous writes to one doc both read the same tree, both
// add their branch, and the second write overwrites the first — both
// clients get a success response, and the lost branch is never re-sent.
//
// The invariant checked here is client-observable and exact: every rev
// the server acknowledged must still be in the tree afterwards
// (`_revs_diff` reports none of them missing), and every pushed
// conflicting branch must show up as a leaf.
//
// Bodies are deliberately large so each write spends real time
// serialising the tree — that's what makes the race reproducible
// instead of a once-in-a-thousand fluke.
//
// No dependencies (native fetch).
// Requires: SERVER_URL/OUR_USER/OUR_PASSWORD (defaults: 127.0.0.1:8085, testuser/testpass)

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const OUR_USER = process.env.OUR_USER || "testuser";
const OUR_PASSWORD = process.env.OUR_PASSWORD || "testpass";
const AUTH = "Basic " + Buffer.from(`${OUR_USER}:${OUR_PASSWORD}`).toString("base64");
const WRITERS = Number(process.env.CONCURRENCY_WRITERS || 24);
const ROUNDS = Number(process.env.CONCURRENCY_ROUNDS || 3);
const PAYLOAD = "x".repeat(Number(process.env.CONCURRENCY_PAYLOAD || 60_000));

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

// Fixed-width index so e.g. writer 1 and writer 10 can't pad to the same hash.
const hash = (round, i, tag) => `${tag}r${round}i${String(i).padStart(4, "0")}`.padEnd(32, "0");

// Every branch hangs off the same root; each writer pushes its own leaf.
async function conflictingPushes(db, docId, round, via) {
  const root = "rootrootrootrootrootrootrootroot";
  await req(db, "PUT", `/${docId}?new_edits=false`, {
    _id: docId,
    _rev: `1-${root}`,
    _revisions: { start: 1, ids: [root] },
    v: 0,
  });
  const branches = Array.from({ length: WRITERS }, (_, i) => hash(round, i, via === "put" ? "p" : "b"));
  const results = await Promise.all(
    branches.map((h) => {
      const doc = { _id: docId, _rev: `2-${h}`, _revisions: { start: 2, ids: [h, root] }, payload: PAYLOAD };
      return via === "put"
        ? req(db, "PUT", `/${docId}?new_edits=false`, doc)
        : req(db, "POST", "/_bulk_docs", { new_edits: false, docs: [doc] });
    })
  );
  const acked = branches
    .filter((_, i) => (via === "put" ? results[i].status === 200 || results[i].status === 201 : Array.isArray(results[i].body) && results[i].body.length === 0))
    .map((h) => `2-${h}`);
  return { acked, total: branches.length };
}

async function missingOf(db, docId, revs) {
  const diff = await req(db, "POST", "/_revs_diff", { [docId]: revs });
  return diff.body[docId]?.missing || [];
}

async function leavesOf(db, docId) {
  const all = await req(db, "GET", `/${docId}?open_revs=all`);
  return Array.isArray(all.body) ? all.body.map((e) => e.ok?._rev).filter(Boolean) : [];
}

async function main() {
  const db = "concurrency_" + Date.now();
  const created = await req(db, "PUT", "");
  if (created.status !== 200 && created.status !== 201) throw new Error(`could not create ${db}: ${created.status}`);

  try {
    for (const via of ["put", "bulk"]) {
      for (let round = 0; round < ROUNDS; round++) {
        const docId = `conflict-${via}-${round}`;
        const { acked, total } = await conflictingPushes(db, docId, round, via);
        const missing = await missingOf(db, docId, acked);
        const leaves = await leavesOf(db, docId);
        check(
          `${total} parallel conflicting ${via === "put" ? "PUT ?new_edits=false" : "_bulk_docs new_edits:false"} pushes (round ${round + 1}): every acknowledged branch survives`,
          acked.length === total && missing.length === 0 && leaves.length === total,
          `acked=${acked.length}/${total} lost=${missing.length} leaves=${leaves.length}`
        );
      }
    }

    // Default (new_edits:true) PUTs: no optimistic concurrency here, so
    // every PUT succeeds and builds on the then-current winner. Each
    // acknowledged rev must still exist — serialised, they form one line.
    for (let round = 0; round < ROUNDS; round++) {
      const docId = `linear-${round}`;
      const results = await Promise.all(
        Array.from({ length: WRITERS }, (_, i) => req(db, "PUT", `/${docId}`, { writer: i, payload: PAYLOAD }))
      );
      const acked = results.filter((r) => r.body?.ok).map((r) => r.body.rev);
      const missing = await missingOf(db, docId, acked);
      const final = await req(db, "GET", `/${docId}`);
      check(
        `${WRITERS} parallel plain PUTs (round ${round + 1}): every acknowledged rev survives, history is one line`,
        acked.length === WRITERS && missing.length === 0 && Number(final.body._rev?.split("-")[0]) === WRITERS,
        `acked=${acked.length} lost=${missing.length} final=${final.body._rev}`
      );
    }
  } finally {
    await req(db, "DELETE", "");
  }

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nNo acknowledged write was lost under concurrency");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
