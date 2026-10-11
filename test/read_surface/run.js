// Read endpoints CouchDB clients and tools expect (see doc/changelog.md,
// v0.1.7): `_all_docs`, `_design/...` ids, `GET /{db}` metadata, and
// `_changes?limit=`.
//
// No dependencies (native fetch).
// Requires: SERVER_URL/OUR_USER/OUR_PASSWORD (defaults: 127.0.0.1:8085, testuser/testpass)

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

const ids = (r) => (r.body?.rows || []).map((row) => row.id ?? row.key);
const q = (v) => encodeURIComponent(JSON.stringify(v));

async function main() {
  const db = "readsurf_" + Date.now();
  await req(db, "PUT", "");
  try {
    for (const id of ["c", "a", "e", "b", "d"]) await req(db, "PUT", `/${id}`, { v: id });
    const gone = await req(db, "PUT", "/gone", { v: 1 });
    await req(db, "DELETE", `/gone?rev=${gone.body.rev}`);

    const all = await req(db, "GET", "/_all_docs");
    check(
      "_all_docs lists live docs sorted by id, with total_rows, without tombstones",
      all.status === 200 && JSON.stringify(ids(all)) === JSON.stringify(["a", "b", "c", "d", "e"]) && all.body.total_rows === 5 &&
        all.body.rows[0].value?.rev?.startsWith("1-"),
      `${all.status} ${JSON.stringify(all.body).slice(0, 200)}`
    );

    const docs = await req(db, "GET", "/_all_docs?include_docs=true&limit=2&skip=1");
    check(
      "_all_docs include_docs, limit and skip",
      JSON.stringify(ids(docs)) === JSON.stringify(["b", "c"]) && docs.body.rows[0].doc?.v === "b" && docs.body.offset === 1,
      JSON.stringify(docs.body).slice(0, 200)
    );

    const range = await req(db, "GET", `/_all_docs?startkey=${q("b")}&endkey=${q("d")}`);
    const desc = await req(db, "GET", `/_all_docs?descending=true&limit=2`);
    check(
      "_all_docs startkey/endkey (inclusive) and descending",
      JSON.stringify(ids(range)) === JSON.stringify(["b", "c", "d"]) && JSON.stringify(ids(desc)) === JSON.stringify(["e", "d"]),
      `${JSON.stringify(ids(range))} ${JSON.stringify(ids(desc))}`
    );

    const keys = await req(db, "POST", "/_all_docs", { keys: ["d", "nope", "gone", "a"] });
    const rows = keys.body?.rows || [];
    check(
      "POST _all_docs {keys} keeps request order; unknown -> error, deleted -> value.deleted",
      rows.length === 4 && rows[0].id === "d" && rows[1].error === "not_found" && rows[2].value?.deleted === true && rows[3].id === "a",
      JSON.stringify(rows).slice(0, 300)
    );

    const ddoc = await req(db, "PUT", "/_design/app", { language: "javascript", views: {} });
    const ddocGet = await req(db, "GET", "/_design/app");
    const ddocDel = await req(db, "DELETE", `/_design/app?rev=${ddoc.body?.rev}`);
    check(
      "_design/<name> docs can be written, read and deleted (ids containing '/')",
      ddoc.status === 200 && ddoc.body?.id === "_design/app" && ddocGet.body?._id === "_design/app" && ddocDel.status === 200,
      `${ddoc.status} ${JSON.stringify(ddoc.body)} / ${ddocGet.status} ${JSON.stringify(ddocGet.body)} / ${ddocDel.status}`
    );

    const info = await req(db, "GET", "");
    const lastSeq = (await req(db, "GET", "/_changes")).body.last_seq;
    check(
      "GET /{db}: doc_count excludes tombstones; doc_del_count, update_seq, instance_start_time present",
      info.body?.doc_count === 5 && info.body?.doc_del_count === 2 && info.body?.update_seq === lastSeq && info.body?.instance_start_time === "0",
      `${JSON.stringify(info.body)} (changes last_seq ${lastSeq})`
    );

    const page1 = await req(db, "GET", "/_changes?limit=2");
    const page2 = await req(db, "GET", `/_changes?since=${page1.body?.last_seq}`);
    const seen = [...(page1.body?.results || []), ...(page2.body?.results || [])].map((r) => r.id);
    check(
      "_changes?limit=2 returns 2 rows, and resuming from its last_seq gets all the rest",
      page1.body?.results?.length === 2 && page1.body.last_seq === page1.body.results[1].seq && new Set(seen).size === 7,
      `page1=${JSON.stringify(page1.body)} seen=${JSON.stringify(seen)}`
    );
  } finally {
    await req(db, "DELETE", "");
  }

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nRead endpoints behave like CouchDB's");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
