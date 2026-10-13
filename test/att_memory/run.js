// Server memory must stay flat however many attachment bytes pass
// through it (see doc/changelog.md, v0.1.8). In v0.1.7 attachment bytes
// lived in sled, whose cache cap doesn't bound large values: resident
// memory grew with every attachment byte (30 -> 159 MB for 120 MB) and
// only a restart freed it.
//
// This test owns its server (port 8289): it writes ATT_MEM_MB (default
// 1024) of 10 MB attachments, then reads every one back, measuring
// resident memory at 25 %, 50 % and 100 % of each phase. The check is
// that memory stops growing: the last 50 % of the data adds little.
//
// Requires a built binary: NYXDB_BIN (default target/release/nyxdb[.exe]).

const crypto = require("node:crypto");
const { check, finish, residentMB, spawnServer } = require("../lib/harness.js");

const PORT = Number(process.env.ATT_MEM_PORT || 8289);
const TOTAL_MB = Number(process.env.ATT_MEM_MB || 1024);
const EACH_MB = 10;
const GROWTH_MB = Number(process.env.ATT_MEM_GROWTH_MB || 40);
const CEILING_MB = Number(process.env.ATT_MEM_CEILING_MB || 250);

async function main() {
  const server = await spawnServer({ port: PORT, user: "attmem", password: "attmem" });
  const url = (p) => `${server.url}/attmem${p}`;
  const headers = { Authorization: server.auth };
  const count = Math.ceil(TOTAL_MB / EACH_MB);
  const marks = new Set([Math.floor(count / 4), Math.floor(count / 2), count].map((n) => n - 1));
  const rss = () => residentMB(server.child.pid);
  try {
    await fetch(url(""), { method: "PUT", headers });
    const write = [];
    const blob = crypto.randomBytes(EACH_MB * 1024 * 1024);
    for (let i = 0; i < count; i++) {
      blob.writeUInt32LE(i, 0); // distinct content, so nothing is deduplicated
      const r = await fetch(url(`/doc${i}/file.bin`), { method: "PUT", headers: { ...headers, "Content-Type": "application/octet-stream" }, body: blob });
      if (!r.ok) throw new Error(`upload ${i}: ${r.status} ${await r.text()}`);
      await r.arrayBuffer();
      if (marks.has(i)) write.push(rss());
    }
    const read = [];
    for (let i = 0; i < count; i++) {
      const r = await fetch(url(`/doc${i}/file.bin`), { headers });
      const got = Buffer.from(await r.arrayBuffer());
      if (got.length !== blob.length || got.readUInt32LE(0) !== i) throw new Error(`read ${i}: ${r.status}, ${got.length} bytes`);
      if (marks.has(i)) read.push(rss());
    }
    const mb = (x) => `${Math.round(x)} MB`;
    console.log(`resident memory, ${TOTAL_MB} MB of attachments: writing ${write.map(mb).join(" -> ")}; reading ${read.map(mb).join(" -> ")}`);
    for (const [phase, [, half, end]] of [["writing", write], ["reading", read]]) {
      check(`${phase}: the second half of ${TOTAL_MB} MB of attachments adds under ${GROWTH_MB} MB of memory`, end - half < GROWTH_MB, `${mb(half)} -> ${mb(end)}`);
      check(`${phase}: memory stays under ${CEILING_MB} MB`, end < CEILING_MB, mb(end));
    }
  } finally {
    await server.stop();
  }
  finish("Memory stays flat while attachments are written and read");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
