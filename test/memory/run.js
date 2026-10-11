// Server memory must stay bounded as the data grows (see
// doc/changelog.md, v0.1.7).
//
// sled's page cache defaults to 1 GB, so NyxDB's memory grew with the data
// it touched: measured ~195 MB resident after writing 100 MB of small docs
// and ~163 MB after reading them back. The cache is now capped (32 MB by
// default, NYXDB_CACHE_MB to tune).
//
// This test owns its server process: write ~100 MB of 2 KB docs, restart,
// read everything back. In each phase memory is measured halfway and at
// the end. The check is that it stops growing — the second half of the
// data adds little — not a fixed number: resident memory is counted
// differently per OS (Windows working set vs Linux RSS with glibc
// arenas), so an absolute limit tuned on one fails on the other. A
// generous ceiling is checked too.
//
// Known limit, documented rather than tested: in sled 0.34 the cap does
// not bound *large* values (~1 MB each) — memory then tracks the data
// touched regardless of the cap. See doc/USAGE.md.
//
// Requires a built binary: NYXDB_BIN (default target/release/nyxdb[.exe]).

const crypto = require("node:crypto");
const fs = require("node:fs");
const { BIN, basicAuth, check, finish, residentMB, spawnServer } = require("../lib/harness.js");

const PORT = Number(process.env.MEMORY_PORT || 8295);
const BASE = `http://127.0.0.1:${PORT}`;
const AUTH = basicAuth("mem", "mem");
const DATA_MB = Number(process.env.MEMORY_DATA_MB || 100);
// Second half of the data may add at most this much (unbounded growth adds ~50 MB).
const GROWTH_MB = Number(process.env.MEMORY_GROWTH_MB || 25);
const CEILING_MB = Number(process.env.MEMORY_CEILING_MB || 200);
const PER_BATCH = 1000; // 2 KB docs -> ~2 MB per request, so request parsing stays small

const start = (dataDir) => spawnServer({ port: PORT, user: "mem", password: "mem", dataDir });

const post = (p, body) =>
  fetch(`${BASE}/mem${p}`, { method: "POST", headers: { Authorization: AUTH, "Content-Type": "application/json" }, body: JSON.stringify(body) });

async function main() {
  if (!fs.existsSync(BIN)) throw new Error(`no server binary at ${BIN} (build it, or set NYXDB_BIN)`);
  const batches = Math.ceil((DATA_MB * 1024) / (2 * PER_BATCH));
  let server = await start();
  const dataDir = server.dataDir;
  try {
    await fetch(`${BASE}/mem`, { method: "PUT", headers: { Authorization: AUTH } });
    const half = Math.floor(batches / 2);
    const write = [];
    for (let b = 0; b < batches; b++) {
      const docs = Array.from({ length: PER_BATCH }, (_, i) => ({ _id: `d${b}-${i}`, blob: crypto.randomBytes(1536).toString("base64") }));
      const r = await post("/_bulk_docs", { docs });
      if (!r.ok) throw new Error(`write failed: ${r.status}`);
      if (b === half - 1) write.push(residentMB(server.child.pid));
    }
    write.push(residentMB(server.child.pid));

    await server.stop({ keepData: true });
    server = await start(dataDir);
    const read = [];
    for (let b = 0; b < batches; b++) {
      for (let off = 0; off < PER_BATCH; off += 1000) {
        const docs = Array.from({ length: 1000 }, (_, i) => ({ id: `d${b}-${off + i}` }));
        await (await post("/_bulk_get", { docs })).arrayBuffer();
      }
      if (b === half - 1) read.push(residentMB(server.child.pid));
    }
    read.push(residentMB(server.child.pid));

    const mb = (x) => `${Math.round(x)} MB`;
    console.log(`resident memory: writing ${mb(write[0])} at ${DATA_MB / 2} MB, ${mb(write[1])} at ${DATA_MB} MB; reading ${mb(read[0])} -> ${mb(read[1])}`);
    for (const [phase, [mid, end]] of [["writing", write], ["reading back (fresh process)", read]]) {
      check(
        `${phase}: the second ${DATA_MB / 2} MB of data adds under ${GROWTH_MB} MB of memory`,
        end - mid < GROWTH_MB,
        `${mb(mid)} -> ${mb(end)}`
      );
      check(`${phase}: memory stays under ${CEILING_MB} MB`, end < CEILING_MB, mb(end));
    }
  } finally {
    await server.stop();
  }

  finish("Server memory stays bounded as data grows");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
