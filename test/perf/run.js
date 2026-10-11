// Throughput guards: catch order-of-magnitude regressions, not small
// drifts (thresholds have a wide margin).
//
// v0.1.7's durability fix made every doc in a `_bulk_docs` its own sled
// transaction. sled 0.34 pays ~0.5 ms per transaction at the next flush,
// and a batch is flushed before it is acknowledged — so a 2,000-doc batch
// took ~1.4 s instead of ~50 ms, 27× slower than v0.1.6 (see
// doc/changelog.md). A replication push (100 docs per request) paid it on
// every batch.
//
// Needs a release build for meaningful numbers.

const http = require("node:http");
const { SERVER_URL, AUTH, check, finish } = require("../lib/harness.js");

// node:http with keep-alive: Node's fetch adds ~12 ms per request on
// Windows loopback, which would hide the server's own time.
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

async function timed(fn) {
  const started = performance.now();
  const result = await fn();
  return { ms: performance.now() - started, result };
}

async function main() {
  const db = "perf_" + Date.now();
  await call("PUT", `/${db}`);
  try {
    const docs = Array.from({ length: 2000 }, (_, i) => ({ _id: `app-${i}`, title: `Task ${i}`, done: i % 2 === 0 }));
    const app = await timed(() => call("POST", `/${db}/_bulk_docs`, { docs }));
    check(
      "a 2,000-doc _bulk_docs (app writes) finishes in under 400 ms",
      app.result.status === 201 || app.result.status === 200 ? app.ms < 400 : false,
      `${app.result.status} in ${Math.round(app.ms)} ms`
    );

    const pushed = Array.from({ length: 2000 }, (_, i) => {
      const hash = i.toString(16).padStart(32, "a");
      return { _id: `push-${i}`, _rev: `1-${hash}`, _revisions: { start: 1, ids: [hash] }, n: i };
    });
    const push = await timed(() => call("POST", `/${db}/_bulk_docs`, { new_edits: false, docs: pushed }));
    check(
      "a 2,000-doc replication push (new_edits:false) finishes in under 400 ms",
      Array.isArray(push.result.body) && push.result.body.length === 0 && push.ms < 400,
      `${push.result.status} ${JSON.stringify(push.result.body).slice(0, 100)} in ${Math.round(push.ms)} ms`
    );

    const changes = await call("GET", `/${db}/_changes`);
    check("all 4,000 docs are in _changes", changes.body.results.length === 4000, `${changes.body.results.length} rows`);
  } finally {
    await call("DELETE", `/${db}`);
  }
  finish("Bulk writes are not slowed by per-document flush cost");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
