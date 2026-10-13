// Server memory must stay bounded as the data grows (see
// doc/changelog.md, v0.1.7).
//
// sled's page cache defaults to 1 GB, so NyxDB's memory grew with the data
// it touched: measured ~195 MB resident after writing 100 MB of small docs
// and ~163 MB after reading them back. The cache is now capped (32 MB by
// default, NYXDB_CACHE_MB to tune).
//
// This test owns its server process: write ~100 MB of 2 KB docs, restart,
// read everything back. In each phase memory is measured at the start and
// after each third of the data. The check is the shape, not a number: a
// bounded cache fills during the first part and then levels off, so the
// last third adds far less than the first; v0.1.6's unbounded cache grew
// at the same rate throughout. Absolute numbers differ per OS (Windows
// working set vs Linux RSS with glibc arenas) and the point where the
// cache is full varies, so a fixed limit or a halfway split was flaky on
// one or the other. A generous ceiling is checked too.
//
// Attachments (files since v0.1.8) are covered by test/att_memory.
//
// Requires a built binary: NYXDB_BIN (default target/release/nyxdb[.exe]).

const crypto = require("node:crypto");
const fs = require("node:fs");
const { BIN, basicAuth, check, finish, residentMB, spawnServer } = require("../lib/harness.js");

const PORT = Number(process.env.MEMORY_PORT || 8295);
const BASE = `http://127.0.0.1:${PORT}`;
const AUTH = basicAuth("mem", "mem");
const DATA_MB = Number(process.env.MEMORY_DATA_MB || 100);
// The last third may add at most half of what the first third added, plus
// this much for allocator noise.
const SLACK_MB = Number(process.env.MEMORY_SLACK_MB || 8);
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
    const marks = new Set([Math.floor(batches / 3), Math.floor((2 * batches) / 3)].map((n) => n - 1));
    const write = [residentMB(server.child.pid)];
    for (let b = 0; b < batches; b++) {
      const docs = Array.from({ length: PER_BATCH }, (_, i) => ({ _id: `d${b}-${i}`, blob: crypto.randomBytes(1536).toString("base64") }));
      const r = await post("/_bulk_docs", { docs });
      if (!r.ok) throw new Error(`write failed: ${r.status}`);
      if (marks.has(b)) write.push(residentMB(server.child.pid));
    }
    write.push(residentMB(server.child.pid));

    await server.stop({ keepData: true });
    server = await start(dataDir);
    const read = [residentMB(server.child.pid)];
    for (let b = 0; b < batches; b++) {
      for (let off = 0; off < PER_BATCH; off += 1000) {
        const docs = Array.from({ length: 1000 }, (_, i) => ({ id: `d${b}-${off + i}` }));
        await (await post("/_bulk_get", { docs })).arrayBuffer();
      }
      if (marks.has(b)) read.push(residentMB(server.child.pid));
    }
    read.push(residentMB(server.child.pid));

    const mb = (x) => `${Math.round(x)} MB`;
    console.log(`resident memory at 0, 1/3, 2/3, all of ${DATA_MB} MB: writing ${write.map(mb).join(" -> ")}; reading ${read.map(mb).join(" -> ")}`);
    for (const [phase, [start, third, twoThirds, end]] of [["writing", write], ["reading back (fresh process)", read]]) {
      const first = third - start;
      const last = end - twoThirds;
      check(
        `${phase}: memory levels off — the last third of the data adds under half what the first third did`,
        last < first / 2 + SLACK_MB,
        `first third +${mb(first)}, last third +${mb(last)}`
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
