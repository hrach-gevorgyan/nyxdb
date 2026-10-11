// Slow storage work must not stall unrelated requests (see
// doc/changelog.md, v0.1.7).
//
// sled calls block the thread they run on. Run directly inside async
// handlers, they occupied tokio's async worker threads (one per CPU), so a
// handful of slow requests — full `_changes` scans of a big database —
// left nothing to answer even `GET /` until they finished. Storage work
// now runs on tokio's blocking pool, leaving the async workers free.
//
// No dependencies (native fetch).
// Requires: SERVER_URL/OUR_USER/OUR_PASSWORD (defaults: 127.0.0.1:8085, testuser/testpass)

const os = require("node:os");

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const OUR_USER = process.env.OUR_USER || "testuser";
const OUR_PASSWORD = process.env.OUR_PASSWORD || "testpass";
const AUTH = "Basic " + Buffer.from(`${OUR_USER}:${OUR_PASSWORD}`).toString("base64");
const DOCS = Number(process.env.BLOCKING_DOCS || 20000);
const SLOW = Number(process.env.BLOCKING_SLOW || os.cpus().length * 4);
const LIMIT_MS = Number(process.env.BLOCKING_LIMIT_MS || 500);

let failures = 0;
function check(name, condition, detail) {
  if (condition) console.log(`PASS: ${name}`);
  else {
    console.error(`FAIL: ${name}${detail ? " — " + detail : ""}`);
    failures++;
  }
}

const req = (db, method, path, body) =>
  fetch(`${SERVER_URL}/${db}${path}`, {
    method,
    headers: { "Content-Type": "application/json", Authorization: AUTH },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });

async function main() {
  const db = "blocking_" + Date.now();
  await req(db, "PUT", "");
  try {
    for (let b = 0; b < DOCS / 1000; b++) {
      const docs = Array.from({ length: 1000 }, (_, i) => ({ _id: `d${b}-${i}`, v: i }));
      await req(db, "POST", "/_bulk_docs", { docs });
    }

    const slow = Array.from({ length: SLOW }, () => req(db, "GET", "/_changes?since=0&style=all_docs").then((r) => r.arrayBuffer()));
    await new Promise((r) => setTimeout(r, 50)); // let them occupy the server
    const latencies = [];
    for (let i = 0; i < 5; i++) {
      const t = Date.now();
      await (await fetch(`${SERVER_URL}/`)).arrayBuffer();
      latencies.push(Date.now() - t);
    }
    const slowStarted = Date.now();
    await Promise.all(slow);
    const worst = Math.max(...latencies);
    check(
      `GET / answers within ${LIMIT_MS} ms while ${SLOW} full _changes scans of ${DOCS} docs run`,
      worst < LIMIT_MS,
      `GET / latencies ${JSON.stringify(latencies)} ms (scans still running for ${Date.now() - slowStarted} ms after)`
    );
  } finally {
    await req(db, "DELETE", "");
  }

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nSlow storage work doesn't stall other requests");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
