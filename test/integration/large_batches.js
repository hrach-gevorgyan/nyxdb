// Large replication batches must not stall sync (see doc/changelog.md,
// v0.1.7).
//
// PouchDB pushes 100 docs per `_bulk_docs` request by default, with
// attachments inline as base64. With a 50 MB body limit, a batch of
// attachment-carrying docs got 413 and replication retried the same
// batch forever. The default limit is now 256 MB (one maximal Offlog doc
// — ten 10 MB attachments, ~133 MB as base64 — fits with room to spare),
// and an over-limit body gets CouchDB's 413 {"error":"too_large"} so the
// client sees why.
//
// Uses SERVER_URL for the PouchDB push and the >100-doc batch (needs the
// server's default limit), and spawns its own small-limit server
// (NYXDB_BIN, port LARGE_PORT) to check behaviour at the limit cheaply.

const PouchDB = require("pouchdb");
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const TEST_USER = process.env.TEST_USER || "testuser";
const TEST_PASSWORD = process.env.TEST_PASSWORD || "testpass";
const AUTH = "Basic " + Buffer.from(`${TEST_USER}:${TEST_PASSWORD}`).toString("base64");
const BIN =
  process.env.NYXDB_BIN ||
  path.join(__dirname, "..", "..", "target", "release", process.platform === "win32" ? "nyxdb.exe" : "nyxdb");
const PORT = Number(process.env.LARGE_PORT || 8294);
const SMALL_LIMIT = 8 * 1024 * 1024;

let failures = 0;
function check(name, condition, detail) {
  if (condition) console.log(`PASS: ${name}`);
  else {
    console.error(`FAIL: ${name}${detail ? " — " + detail : ""}`);
    failures++;
  }
}

async function bulk(base, auth, db, docs) {
  const resp = await fetch(`${base}/${db}/_bulk_docs`, {
    method: "POST",
    headers: { Authorization: auth, "Content-Type": "application/json" },
    body: JSON.stringify({ docs }),
  });
  return { status: resp.status, body: await resp.json().catch(() => null) };
}

async function sharedServerChecks() {
  const db = "largebatch_" + Date.now();
  await fetch(`${SERVER_URL}/${db}`, { method: "PUT", headers: { Authorization: AUTH } });
  const local = new PouchDB(db + "_local");
  try {
    const many = await bulk(SERVER_URL, AUTH, db, Array.from({ length: 250 }, (_, i) => ({ _id: `small${i}`, i })));
    check(
      "a _bulk_docs batch of 250 docs (over PouchDB's 100) is accepted",
      many.status === 200 && Array.isArray(many.body) && many.body.length === 250 && many.body.every((r) => r.ok),
      `${many.status}`
    );

    const blob = Buffer.alloc(600 * 1024, 1);
    await local.bulkDocs(
      Array.from({ length: 120 }, (_, i) => ({
        _id: `att${i}`,
        _attachments: { "f.bin": { content_type: "application/octet-stream", data: blob.toString("base64") } },
      }))
    );
    const remote = new PouchDB(`${SERVER_URL}/${db}`, { auth: { username: TEST_USER, password: TEST_PASSWORD } });
    const timeout = new Promise((_, reject) => setTimeout(() => reject(new Error("push timed out")), 60000));
    let result = null;
    let error = null;
    try {
      result = await Promise.race([PouchDB.replicate(local, remote), timeout]);
    } catch (err) {
      error = err;
    }
    check(
      "PouchDB pushes 120 docs with 600 KB attachments (~80 MB per 100-doc batch) in one go",
      !error && result?.docs_written === 120 && (result?.doc_write_failures ?? 0) === 0,
      error ? `${error.status || ""} ${error.message}` : JSON.stringify({ written: result?.docs_written, failures: result?.doc_write_failures })
    );
  } finally {
    await local.destroy();
    await fetch(`${SERVER_URL}/${db}`, { method: "DELETE", headers: { Authorization: AUTH } });
  }
}

async function limitChecks() {
  if (!fs.existsSync(BIN)) throw new Error(`no server binary at ${BIN} (build it, or set NYXDB_BIN)`);
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "nyxdb-limit-"));
  const base = `http://127.0.0.1:${PORT}`;
  const auth = "Basic " + Buffer.from("lim:lim").toString("base64");
  const child = spawn(BIN, [], {
    env: {
      ...process.env,
      NYXDB_USER: "lim",
      NYXDB_PASSWORD: "lim",
      NYXDB_DATA: dataDir,
      NYXDB_ADDR: `127.0.0.1:${PORT}`,
      NYXDB_MAX_BODY_BYTES: String(SMALL_LIMIT),
    },
    stdio: "ignore",
  });
  const exited = new Promise((r) => child.once("exit", r));
  try {
    for (let i = 0; i < 100; i++) {
      try {
        if ((await fetch(`${base}/`)).ok) break;
      } catch {}
      await new Promise((r) => setTimeout(r, 50));
    }
    await fetch(`${base}/lim`, { method: "PUT", headers: { Authorization: auth } });
    const sized = (bytes) => [{ _id: "big", pad: "x".repeat(bytes - 100) }];
    const under = await bulk(base, auth, "lim", sized(SMALL_LIMIT - 64 * 1024));
    check("a body just under NYXDB_MAX_BODY_BYTES is accepted", under.status === 200, `${under.status} ${JSON.stringify(under.body)}`);
    const over = await bulk(base, auth, "lim", sized(SMALL_LIMIT + 64 * 1024));
    check(
      "a body over the limit gets 413 {\"error\":\"too_large\"}",
      over.status === 413 && over.body?.error === "too_large",
      `${over.status} ${JSON.stringify(over.body)}`
    );
  } finally {
    child.kill("SIGKILL");
    await exited;
    fs.rmSync(dataDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
}

async function main() {
  await sharedServerChecks();
  await limitChecks();
  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nLarge batches go through; over-limit bodies get a clean 413");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
