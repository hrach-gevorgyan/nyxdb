// Server memory must stay bounded as the data grows (see
// doc/changelog.md, v0.1.7).
//
// sled's page cache defaults to 1 GB, so NyxDB's memory grew with the data
// it touched: measured ~195 MB resident after writing 100 MB of small docs
// and ~163 MB after reading them back. The cache is now capped (64 MB by
// default, NYXDB_CACHE_MB to tune).
//
// This test owns its server process: write ~100 MB of 2 KB docs, restart,
// read everything back, and check resident memory after each phase.
//
// Known limit, documented rather than tested: in sled 0.34 the cap does
// not bound *large* values (~1 MB each) — memory then tracks the data
// touched regardless of the cap. See doc/USAGE.md.
//
// Requires a built binary: NYXDB_BIN (default target/release/nyxdb[.exe]).

const { spawn, execSync } = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const BIN =
  process.env.NYXDB_BIN ||
  path.join(__dirname, "..", "..", "target", "release", process.platform === "win32" ? "nyxdb.exe" : "nyxdb");
const PORT = Number(process.env.MEMORY_PORT || 8295);
const BASE = `http://127.0.0.1:${PORT}`;
const AUTH = "Basic " + Buffer.from("mem:mem").toString("base64");
const DATA_MB = Number(process.env.MEMORY_DATA_MB || 100);
const LIMIT_MB = Number(process.env.MEMORY_LIMIT_MB || 120);
const PER_BATCH = 1000; // 2 KB docs -> ~2 MB per request, so request parsing stays small

let failures = 0;
function check(name, condition, detail) {
  if (condition) console.log(`PASS: ${name}`);
  else {
    console.error(`FAIL: ${name}${detail ? " — " + detail : ""}`);
    failures++;
  }
}

function residentMB(pid) {
  if (process.platform === "win32") {
    return Number(execSync(`powershell -NoProfile -Command "(Get-Process -Id ${pid}).WorkingSet64"`).toString()) / 1048576;
  }
  return Number(fs.readFileSync(`/proc/${pid}/status`, "utf8").match(/VmRSS:\s+(\d+)/)[1]) / 1024;
}

async function start(dataDir) {
  const child = spawn(BIN, [], {
    env: { ...process.env, NYXDB_USER: "mem", NYXDB_PASSWORD: "mem", NYXDB_DATA: dataDir, NYXDB_ADDR: `127.0.0.1:${PORT}` },
    stdio: "ignore",
  });
  const exited = new Promise((r) => child.once("exit", r));
  for (let i = 0; i < 200; i++) {
    try {
      if ((await fetch(`${BASE}/`)).ok) return { child, exited };
    } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  child.kill("SIGKILL");
  throw new Error(`server did not start on port ${PORT}`);
}

async function stop(server) {
  server.child.kill("SIGKILL");
  await server.exited;
}

const post = (p, body) =>
  fetch(`${BASE}/mem${p}`, { method: "POST", headers: { Authorization: AUTH, "Content-Type": "application/json" }, body: JSON.stringify(body) });

async function main() {
  if (!fs.existsSync(BIN)) throw new Error(`no server binary at ${BIN} (build it, or set NYXDB_BIN)`);
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "nyxdb-memory-"));
  const batches = Math.ceil((DATA_MB * 1024) / (2 * PER_BATCH));
  let server = await start(dataDir);
  try {
    await fetch(`${BASE}/mem`, { method: "PUT", headers: { Authorization: AUTH } });
    for (let b = 0; b < batches; b++) {
      const docs = Array.from({ length: PER_BATCH }, (_, i) => ({ _id: `d${b}-${i}`, blob: crypto.randomBytes(1536).toString("base64") }));
      const r = await post("/_bulk_docs", { docs });
      if (!r.ok) throw new Error(`write failed: ${r.status}`);
    }
    const afterWrite = residentMB(server.child.pid);

    await stop(server);
    server = await start(dataDir);
    for (let b = 0; b < batches; b++) {
      for (let off = 0; off < PER_BATCH; off += 1000) {
        const docs = Array.from({ length: 1000 }, (_, i) => ({ id: `d${b}-${off + i}` }));
        await (await post("/_bulk_get", { docs })).arrayBuffer();
      }
    }
    const afterRead = residentMB(server.child.pid);

    check(
      `after writing ~${DATA_MB} MB of small docs, memory stays under ${LIMIT_MB} MB`,
      afterWrite < LIMIT_MB,
      `${Math.round(afterWrite)} MB resident`
    );
    check(
      `after reading all of it back (fresh process), memory stays under ${LIMIT_MB} MB`,
      afterRead < LIMIT_MB,
      `${Math.round(afterRead)} MB resident`
    );
  } finally {
    await stop(server);
    fs.rmSync(dataDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nServer memory stays bounded as data grows");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
