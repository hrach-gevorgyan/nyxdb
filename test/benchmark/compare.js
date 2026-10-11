// Side-by-side benchmark: any number of NyxDB binaries (e.g. this build
// and an older release) and, optionally, a real CouchDB — identical
// workloads, median of BENCH_REPS runs. Prints a Markdown table.
// Numbers, not pass/fail.
//
//   BENCH_NYXDB="v0.1.7=target/release/nyxdb.exe,v0.1.6=/path/to/old/nyxdb.exe"
//   COUCH_URL=http://127.0.0.1:5984 COUCH_USER=admin COUCH_PASSWORD=secret   (optional)
//   BENCH_DOCS=5000 BENCH_REPS=3 BENCH_PORT=8290
//   node test/benchmark/compare.js
//
// Each NyxDB is spawned fresh per repetition on BENCH_PORT with a temp
// data dir (deleted afterwards). CouchDB is used as running, with
// disposable `bench_*` databases that are deleted afterwards. Needs
// `npm install --prefix test/integration` (the PouchDB pull workload).

const { execSync } = require("node:child_process");
const fs = require("node:fs");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");
const PouchDB = require("../integration/pouchdb");
const { basicAuth, residentMB, sleep, spawnServer } = require("../lib/harness.js");

const agent = new http.Agent({ keepAlive: true, maxSockets: 16 });
const DOCS = Number(process.env.BENCH_DOCS || 5000);
const REPS = Number(process.env.BENCH_REPS || 3);
const PORT = Number(process.env.BENCH_PORT || 8290);
const SINGLE_WRITES = 300;
const CONCURRENT_WORKERS = 8;
const CONCURRENT_PER_WORKER = 50;
const READS = 300;
const ATTACHMENTS = 20;
const ATTACHMENT_BYTES = 512 * 1024;

const targets = (process.env.BENCH_NYXDB || `this build=${path.join(__dirname, "..", "..", "target", "release", process.platform === "win32" ? "nyxdb.exe" : "nyxdb")}`)
  .split(",")
  .map((pair) => {
    const [label, bin] = pair.split("=");
    return { kind: "nyxdb", label, bin: path.resolve(bin) };
  });
if (process.env.COUCH_USER) {
  targets.push({ kind: "couchdb", label: "CouchDB", url: process.env.COUCH_URL || "http://127.0.0.1:5984", auth: basicAuth(process.env.COUCH_USER, process.env.COUCH_PASSWORD), creds: { username: process.env.COUCH_USER, password: process.env.COUCH_PASSWORD } });
}

// A task-manager-like record (what this server is for), not a toy {"a":1}.
const makeDoc = (i) => ({
  _id: `doc-${String(i).padStart(6, "0")}`,
  title: `Task number ${i}`,
  done: i % 3 === 0,
  tags: ["work", "personal", "urgent"].slice(0, (i % 3) + 1),
  createdAt: new Date(2026, 0, 1 + (i % 28)).toISOString(),
  notes: "Some free-text notes describing this task in a bit more detail than just a title.",
});

function dirBytes(dir) {
  let total = 0;
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    total += e.isDirectory() ? dirBytes(p) : fs.statSync(p).size;
  }
  return total;
}

/// Resident memory of the CouchDB service (its Erlang VM processes).
function couchMB() {
  if (process.platform !== "win32") return null;
  const out = execSync(
    `powershell -NoProfile -Command "(Get-Process -Name erl,beam*,epmd,couchdb* -ErrorAction SilentlyContinue | Measure-Object WorkingSet64 -Sum).Sum"`
  ).toString();
  return Number(out) / 1048576 || null;
}

const median = (xs) => {
  const v = xs.filter((x) => x !== null && x !== undefined && !Number.isNaN(x)).sort((a, b) => a - b);
  return v.length ? v[Math.floor(v.length / 2)] : null;
};

async function runOnce(target) {
  const m = {};
  let server = null;
  let base;
  let auth;
  let creds = { username: "bench", password: "bench-pass" };
  if (target.kind === "nyxdb") {
    const t0 = Date.now();
    server = await spawnServer({ port: PORT, user: "bench", password: "bench-pass", bin: target.bin });
    m.startupMs = Date.now() - t0;
    base = server.url;
    auth = server.auth;
    await sleep(500);
    m.idleMB = residentMB(server.child.pid);
  } else {
    base = target.url;
    auth = target.auth;
    creds = target.creds;
    m.idleMB = couchMB();
  }
  const db = `bench_${Date.now()}`;
  // node:http with keep-alive, not fetch: on Windows, Node's fetch adds
  // ~12 ms per request on loopback, which would swamp every server-side
  // difference (measured: 0.4 ms vs 13 ms for the same GET).
  const call = (method, p, body, headers = {}) =>
    new Promise((resolve, reject) => {
      const payload = body === undefined ? undefined : Buffer.isBuffer(body) ? body : Buffer.from(JSON.stringify(body));
      const request = http.request(
        `${base}/${db}${p}`,
        {
          method,
          agent,
          headers: { "Content-Type": "application/json", Authorization: auth, ...(payload ? { "Content-Length": payload.length } : {}), ...headers },
        },
        (resp) => {
          const chunks = [];
          resp.on("data", (c) => chunks.push(c));
          resp.on("end", () => {
            const text = Buffer.concat(chunks).toString();
            if (resp.statusCode >= 400) return reject(new Error(`${target.label} ${method} ${p}: ${resp.statusCode} ${text.slice(0, 200)}`));
            resolve(text ? JSON.parse(text) : null);
          });
        }
      );
      request.on("error", reject);
      request.end(payload);
    });
  const time = async (fn) => {
    const t = process.hrtime.bigint();
    await fn();
    return Number(process.hrtime.bigint() - t) / 1e6;
  };

  try {
    await call("PUT", "");
    const docs = Array.from({ length: DOCS }, (_, i) => makeDoc(i));

    m.bulkWriteMs = await time(() => call("POST", "/_bulk_docs", { docs }));

    m.singleWriteMs = (await time(async () => {
      for (let i = 0; i < SINGLE_WRITES; i++) await call("PUT", `/single-${i}`, makeDoc(i));
    })) / SINGLE_WRITES;

    const concurrentMs = await time(() =>
      Promise.all(
        Array.from({ length: CONCURRENT_WORKERS }, async (_, w) => {
          for (let i = 0; i < CONCURRENT_PER_WORKER; i++) await call("PUT", `/conc-${w}-${i}`, makeDoc(i));
        })
      )
    );
    m.concurrentWritesPerSec = (CONCURRENT_WORKERS * CONCURRENT_PER_WORKER) / (concurrentMs / 1000);

    const step = Math.floor(DOCS / READS);
    m.readMs = (await time(async () => {
      for (let i = 0; i < READS; i++) await call("GET", `/${makeDoc(i * step)._id}`);
    })) / READS;

    m.changesMs = await time(() => call("GET", "/_changes?since=0"));
    m.bulkGetMs = await time(() => call("POST", "/_bulk_get?revs=true", { docs: docs.slice(0, 1000).map((d) => ({ id: d._id })) }));

    const local = new PouchDB(`benchpull_${Date.now()}`);
    try {
      m.pouchPullMs = await time(() =>
        local.replicate.from(new PouchDB(`${base}/${db}`, { auth: creds }))
      );
    } finally {
      await local.destroy().catch(() => {});
    }

    const blob = Buffer.alloc(ATTACHMENT_BYTES, 7);
    const attMs = await time(async () => {
      for (let i = 0; i < ATTACHMENTS; i++) {
        blob.writeUInt32LE(i, 0); // distinct content per attachment
        await call("PUT", `/att-${i}/file.bin`, blob, { "Content-Type": "application/octet-stream" });
      }
    });
    m.attachmentMBps = (ATTACHMENTS * ATTACHMENT_BYTES) / 1048576 / (attMs / 1000);

    await sleep(1000);
    if (target.kind === "nyxdb") {
      m.loadedMB = residentMB(server.child.pid);
      m.diskMB = dirBytes(server.dataDir) / 1048576;
    } else {
      m.loadedMB = couchMB();
      const info = await call("GET", "");
      m.diskMB = (info.sizes?.file ?? 0) / 1048576;
    }
  } finally {
    if (server) await server.stop();
    else await call("DELETE", "").catch(() => {});
  }
  return m;
}

const ROWS = [
  ["Bulk write (one `_bulk_docs`)", (m) => DOCS / (m.bulkWriteMs / 1000), "docs/s", true],
  ["Single-doc `PUT`, sequential", (m) => m.singleWriteMs, "ms/write", false],
  [`Concurrent \`PUT\` (${CONCURRENT_WORKERS} clients)`, (m) => m.concurrentWritesPerSec, "writes/s", true],
  ["Single-doc `GET`, sequential", (m) => m.readMs, "ms/read", false],
  [`Full \`_changes\` (${DOCS + SINGLE_WRITES + CONCURRENT_WORKERS * CONCURRENT_PER_WORKER} docs)`, (m) => m.changesMs, "ms", false],
  ["`_bulk_get` of 1,000 docs", (m) => m.bulkGetMs, "ms", false],
  [`PouchDB pull of the whole db`, (m) => m.pouchPullMs / 1000, "s", false],
  [`Attachment upload (${ATTACHMENTS} × ${ATTACHMENT_BYTES / 1024} KB)`, (m) => m.attachmentMBps, "MB/s", true],
  ["Idle memory", (m) => m.idleMB, "MB", false],
  ["Memory after the workload", (m) => m.loadedMB, "MB", false],
  ["Disk after the workload", (m) => m.diskMB, "MB", false],
  ["Startup to first response", (m) => m.startupMs, "ms", false],
];

const fmt = (v) => (v === null || v === undefined ? "—" : v >= 100 ? Math.round(v).toLocaleString("en-US") : v >= 10 ? v.toFixed(1) : v.toFixed(2));

async function main() {
  for (const t of targets) if (t.kind === "nyxdb" && !fs.existsSync(t.bin)) throw new Error(`no binary at ${t.bin}`);
  const runs = Object.fromEntries(targets.map((t) => [t.label, []]));
  for (let rep = 1; rep <= REPS; rep++) {
    for (const t of targets) {
      process.stderr.write(`rep ${rep}/${REPS}: ${t.label}... `);
      runs[t.label].push(await runOnce(t));
      process.stderr.write("done\n");
    }
  }
  const header = ["", ...targets.map((t) => t.label)];
  console.log(`\n${DOCS} docs, median of ${REPS} runs, ${os.cpus()[0].model.trim()}, ${os.platform()} ${os.release()}, node ${process.version}\n`);
  console.log(`| ${header.join(" | ")} |`);
  console.log(`|${header.map(() => "---").join("|")}|`);
  for (const [name, get, unit] of ROWS) {
    const cells = targets.map((t) => median(runs[t.label].map((m) => { try { return get(m); } catch { return null; } })));
    console.log(`| ${name} (${unit}) | ${cells.map(fmt).join(" | ")} |`);
  }
  for (const t of targets.filter((t) => t.kind === "nyxdb")) {
    console.log(`\n${t.label} binary: ${(fs.statSync(t.bin).size / 1048576).toFixed(2)} MB`);
  }
  if (process.env.BENCH_JSON) fs.writeFileSync(process.env.BENCH_JSON, JSON.stringify({ targets: targets.map((t) => t.label), runs }, null, 2));
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
