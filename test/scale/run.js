// GET /{db} and _all_docs must not get slower as a database grows (see
// doc/changelog.md, v0.1.8). In v0.1.7 both parsed every doc and
// tombstone ever written on every call.
//
// Times each request (median of 25) on a database of 2,000 live docs +
// 500 tombstones, then again after growing it to 40,000 + 10,000. The
// large case may be at most 3x slower (it was ~20x).
//
// Uses node:http with keep-alive: Node's fetch adds ~12 ms per request on
// Windows loopback, which would hide the server's own time.

const http = require("node:http");
const { SERVER_URL, AUTH, check, finish } = require("../lib/harness.js");

const agent = new http.Agent({ keepAlive: true });
function call(method, path, body) {
  return new Promise((resolve, reject) => {
    const payload = body === undefined ? undefined : Buffer.from(JSON.stringify(body));
    const request = http.request(
      `${SERVER_URL}${path}`,
      { method, agent, headers: { Authorization: AUTH, "Content-Type": "application/json", ...(payload ? { "Content-Length": payload.length } : {}) } },
      (resp) => {
        const chunks = [];
        resp.on("data", (c) => chunks.push(c));
        resp.on("end", () => resolve({ status: resp.statusCode, body: JSON.parse(Buffer.concat(chunks).toString() || "null") }));
      }
    );
    request.on("error", reject);
    request.end(payload);
  });
}

async function medianMs(path) {
  const times = [];
  for (let i = 0; i < 25; i++) {
    const t = performance.now();
    const r = await call("GET", path);
    if (r.status !== 200) throw new Error(`${path}: ${r.status}`);
    times.push(performance.now() - t);
  }
  return times.sort((a, b) => a - b)[12];
}

async function grow(db, from, to, deletedEvery) {
  for (let start = from; start < to; start += 1000) {
    const docs = Array.from({ length: Math.min(1000, to - start) }, (_, i) => {
      const n = start + i;
      const hash = n.toString(16).padStart(32, "a");
      return { _id: `doc-${String(n).padStart(6, "0")}`, _rev: `1-${hash}`, _revisions: { start: 1, ids: [hash] }, _deleted: n % deletedEvery === 0, title: `Task ${n}` };
    });
    const r = await call("POST", `/${db}/_bulk_docs`, { new_edits: false, docs });
    if (r.status >= 300) throw new Error(`bulk: ${r.status}`);
  }
}

async function main() {
  const db = "scale_" + Date.now();
  await call("PUT", `/${db}`);
  const paths = {
    "GET /{db}": `/${db}`,
    "_all_docs?limit=10": `/${db}/_all_docs?limit=10`,
    "_all_docs?startkey=…&limit=10": `/${db}/_all_docs?startkey=${encodeURIComponent('"doc-001000"')}&limit=10`,
  };
  try {
    await grow(db, 0, 2500, 5);
    const small = {};
    for (const [name, p] of Object.entries(paths)) small[name] = await medianMs(p);
    const info = (await call("GET", `/${db}`)).body;
    check("doc_count/doc_del_count at 2,500 docs", info.doc_count === 2000 && info.doc_del_count === 500, JSON.stringify(info));

    await grow(db, 2500, 50000, 5);
    const large = {};
    for (const [name, p] of Object.entries(paths)) large[name] = await medianMs(p);
    const big = (await call("GET", `/${db}`)).body;
    check("doc_count/doc_del_count at 50,000 docs", big.doc_count === 40000 && big.doc_del_count === 10000, JSON.stringify(big));

    for (const name of Object.keys(paths)) {
      check(
        `${name} at 50,000 docs is at most 3x its time at 2,500`,
        large[name] < 3 * small[name] + 2,
        `${small[name].toFixed(2)} ms -> ${large[name].toFixed(2)} ms`
      );
    }
    const page = (await call("GET", paths["_all_docs?startkey=…&limit=10"])).body;
    check(
      "_all_docs startkey/limit returns the right live rows",
      page.rows.length === 10 && page.rows[0].id === "doc-001001" && page.total_rows === 40000 && page.rows.every((r) => !r.id.endsWith("0") || Number(r.id.slice(4)) % 5 !== 0),
      JSON.stringify(page).slice(0, 200)
    );
  } finally {
    await call("DELETE", `/${db}`);
    agent.destroy();
  }
  finish("Database info and _all_docs don't slow down as the database grows");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
