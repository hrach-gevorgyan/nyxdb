// Upgrading from v0.1.7 moves attachment bytes out of sled into files
// (see doc/changelog.md, v0.1.8). The move must survive being killed at
// any point and must never lose an attachment.
//
// 1. The real v0.1.7 binary (NYXDB_OLD_BIN) writes a database with many
//    attachments: standalone uploads, inline ones, a duplicate, and one on
//    a conflict branch.
// 2. This build starts on that data and is hard-killed as soon as its
//    migration reports progress, twice.
// 3. A final start must finish the migration; every attachment must read
//    back byte for byte, each content must be one file, no temp file left.
//
// Needs both binaries: NYXDB_OLD_BIN (v0.1.7; test/run_all.js builds it)
// and NYXDB_BIN (this build). Port 8287.

const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const { check, finish, removeDir, sleep, spawnServer } = require("../lib/harness.js");

const PORT = Number(process.env.MIGRATION_PORT || 8287);
const OLD_BIN = process.env.NYXDB_OLD_BIN;
const COUNT = Number(process.env.MIGRATION_ATTACHMENTS || 200);
const SIZE = 256 * 1024;

const sha = (buf) => crypto.createHash("sha256").update(buf).digest("hex");

async function call(server, method, p, body, type = "application/json") {
  const r = await fetch(`${server.url}/mig${p}`, {
    method,
    headers: { Authorization: server.auth, "Content-Type": type },
    body: body === undefined ? undefined : Buffer.isBuffer(body) ? body : JSON.stringify(body),
  });
  return { status: r.status, bytes: Buffer.from(await r.arrayBuffer()) };
}

/// Starts this build with its output in `logFile`; resolves once it answers.
async function startNew(dataDir, logFile) {
  const log = fs.openSync(logFile, "a");
  const server = await spawnServer({ port: PORT, user: "mig", password: "mig", dataDir, stdio: ["ignore", log, log], env: { NO_COLOR: "1" } });
  return { ...server, close: () => fs.closeSync(log) };
}

async function waitForLog(logFile, pattern, ms) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    const text = fs.existsSync(logFile) ? fs.readFileSync(logFile, "utf8") : "";
    const m = text.match(pattern);
    if (m) return m;
    await sleep(20);
  }
  return null;
}

async function main() {
  if (!OLD_BIN || !fs.existsSync(OLD_BIN)) throw new Error(`set NYXDB_OLD_BIN to a v0.1.7 binary (got ${OLD_BIN})`);
  const expected = new Map(); // "docid[?rev=]" -> sha256 of its attachment
  const contents = new Set();

  // 1. v0.1.7 writes the data.
  const old = await spawnServer({ port: PORT, user: "mig", password: "mig", bin: OLD_BIN });
  const dataDir = old.dataDir;
  const logFile = path.join(path.dirname(dataDir), path.basename(dataDir) + ".log");
  try {
    await call(old, "PUT", "");
    for (let i = 0; i < COUNT; i++) {
      const bytes = crypto.randomBytes(SIZE);
      const r = await call(old, "PUT", `/up${i}/file.bin`, bytes, "application/octet-stream");
      if (r.status >= 300) throw new Error(`v0.1.7 upload ${i}: ${r.status}`);
      expected.set(`up${i}`, sha(bytes));
      contents.add(sha(bytes));
    }
    const inline = crypto.randomBytes(SIZE);
    const docs = [0, 1].map((i) => ({ _id: `inline${i}`, _attachments: { "file.bin": { content_type: "application/octet-stream", data: inline.toString("base64") } } }));
    await call(old, "POST", "/_bulk_docs", { docs }); // same content twice: stored once
    expected.set("inline0", sha(inline));
    expected.set("inline1", sha(inline));
    contents.add(sha(inline));
    const branch = crypto.randomBytes(SIZE);
    const h = (c) => c.repeat(32);
    await call(old, "POST", "/_bulk_docs", {
      new_edits: false,
      docs: [
        { _id: "conflicted", _rev: `1-${h("a")}`, _revisions: { start: 1, ids: [h("a")] } },
        { _id: "conflicted", _rev: `2-${h("b")}`, _revisions: { start: 2, ids: [h("b"), h("a")] }, _attachments: { "file.bin": { content_type: "application/octet-stream", data: branch.toString("base64") } } },
        { _id: "conflicted", _rev: `2-${h("c")}`, _revisions: { start: 2, ids: [h("c"), h("a")] }, n: 1 },
      ],
    });
    expected.set(`conflicted?rev=2-${h("b")}`, sha(branch));
    contents.add(sha(branch));
  } finally {
    await old.stop({ keepData: true });
  }

  try {
    // 2. Kill this build mid-migration, twice.
    for (const [round, nth] of [[1, 1], [2, 3]]) {
      fs.writeFileSync(logFile, "");
      const server = await startNew(dataDir, logFile);
      const progress = await waitForLog(logFile, new RegExp(`(migrat\\w+ .*attachment[^\\n]*\\n[\\s\\S]*){${nth}}`), 30000);
      await server.stop({ keepData: true });
      server.close();
      check(`round ${round}: the migration reported progress before the hard kill`, progress !== null, fs.readFileSync(logFile, "utf8").slice(-400));
    }

    // 3. A clean start finishes it.
    fs.writeFileSync(logFile, "");
    const server = await startNew(dataDir, logFile);
    try {
      const done = await waitForLog(logFile, /attachment migration finished/, 60000);
      check("after two kills, a restart finishes the attachment migration", done !== null, fs.readFileSync(logFile, "utf8").slice(-400));
      const lost = [];
      for (const [doc, digest] of expected) {
        const [id, query] = doc.split("?");
        const r = await call(server, "GET", `/${id}/file.bin${query ? "?" + query : ""}`);
        if (r.status !== 200 || sha(r.bytes) !== digest) lost.push(`${doc} (${r.status})`);
      }
      check(`all ${expected.size} attachments read back byte for byte, conflict branch included`, lost.length === 0, lost.slice(0, 5).join(", "));
      const attDir = path.join(dataDir, "att", "mig");
      const files = fs.existsSync(attDir) ? fs.readdirSync(attDir) : [];
      const stored = files.filter((f) => /^[0-9a-f]{64}$/.test(f));
      check(
        `each of the ${contents.size} distinct contents is exactly one file named by its sha256`,
        stored.length === contents.size && [...contents].every((c) => stored.includes(c)),
        `${stored.length} content files in ${attDir}`
      );
      check("no temp file is left behind", files.length === stored.length, files.filter((f) => !stored.includes(f)).join(", "));
    } finally {
      await server.stop();
      server.close();
    }
  } finally {
    removeDir(dataDir);
    fs.rmSync(logFile, { force: true });
  }
  finish("Attachments survive a migration killed midway");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
