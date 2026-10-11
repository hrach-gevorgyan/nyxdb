// Replication must resume from its checkpoint after the server restarts
// (see doc/changelog.md, v0.1.7).
//
// PouchDB's replication id includes the server's `uuid` from `GET /`. If
// that uuid changes on every start, every restart gives every client a
// new replication id, so it finds no checkpoint and starts over from
// `since=0` — re-scanning (and re-diffing) the whole database each time.
//
// This test owns its server process (own port, own data dir): pull 20
// docs, restart the server on the same data, pull again, and check —
// through PouchDB's own `fetch` hook — that the second pull asked for
// `_changes` from where the first left off, not from 0.
//
// Requires a built binary: NYXDB_BIN (default target/release/nyxdb[.exe]).

const PouchDB = require("pouchdb");
const { spawn } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const BIN =
  process.env.NYXDB_BIN ||
  path.join(__dirname, "..", "..", "target", "release", process.platform === "win32" ? "nyxdb.exe" : "nyxdb");
const PORT = Number(process.env.CHECKPOINT_PORT || 8298);
const BASE = `http://127.0.0.1:${PORT}`;
const USER = "ckpt";
const PASS = "ckpt-pass";
const AUTH = "Basic " + Buffer.from(`${USER}:${PASS}`).toString("base64");
const DB = "resume";

let failures = 0;
function check(name, condition, detail) {
  if (condition) console.log(`PASS: ${name}`);
  else {
    console.error(`FAIL: ${name}${detail ? " — " + detail : ""}`);
    failures++;
  }
}

async function startServer(dataDir) {
  const child = spawn(BIN, [], {
    env: { ...process.env, NYXDB_USER: USER, NYXDB_PASSWORD: PASS, NYXDB_DATA: dataDir, NYXDB_ADDR: `127.0.0.1:${PORT}` },
    stdio: "ignore",
  });
  const exited = new Promise((resolve) => child.once("exit", resolve));
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`${BASE}/`)).ok) return { child, exited };
    } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  child.kill("SIGKILL");
  throw new Error(`server did not start on port ${PORT}`);
}

async function stopServer(server) {
  server.child.kill("SIGKILL");
  await server.exited;
}

async function main() {
  if (!fs.existsSync(BIN)) throw new Error(`no server binary at ${BIN} (build it, or set NYXDB_BIN)`);
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "nyxdb-checkpoint-"));
  const localName = "ckpt_local_" + Date.now();
  let server = await startServer(dataDir);
  const local = new PouchDB(localName);
  try {
    await fetch(`${BASE}/${DB}`, { method: "PUT", headers: { Authorization: AUTH } });
    const docs = Array.from({ length: 20 }, (_, i) => ({ _id: `doc${i}`, i }));
    await fetch(`${BASE}/${DB}/_bulk_docs`, {
      method: "POST",
      headers: { Authorization: AUTH, "Content-Type": "application/json" },
      body: JSON.stringify({ docs }),
    });

    // Every _changes request the replicator makes, with its `since`.
    const changesSince = [];
    const remote = new PouchDB(`${BASE}/${DB}`, {
      auth: { username: USER, password: PASS },
      fetch: (url, opts) => {
        const u = new URL(url);
        if (u.pathname.endsWith("/_changes")) changesSince.push(u.searchParams.get("since"));
        return PouchDB.fetch(url, opts);
      },
    });

    const uuidBefore = (await (await fetch(`${BASE}/`)).json()).uuid;
    const first = await PouchDB.replicate(remote, local);
    check("first pull copies all 20 docs", first.docs_written === 20, JSON.stringify(first));

    await stopServer(server);
    server = await startServer(dataDir);
    const uuidAfter = (await (await fetch(`${BASE}/`)).json()).uuid;
    check("server uuid is the same after a restart", uuidBefore === uuidAfter, `${uuidBefore} -> ${uuidAfter}`);

    changesSince.length = 0;
    const second = await PouchDB.replicate(remote, local);
    const restarted = changesSince.filter((s) => s === "0" || s === null);
    check(
      "the pull after a restart resumes from its checkpoint instead of since=0",
      changesSince.length > 0 && restarted.length === 0 && second.docs_written === 0,
      `_changes since values: ${JSON.stringify(changesSince)}, docs_written=${second.docs_written}`
    );
  } finally {
    await stopServer(server);
    // LevelDB can still hold its LOG file for a moment after replication
    // settles on Windows; close, then remove the directory with retries.
    await local.close().catch(() => {});
    fs.rmSync(localName, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
    fs.rmSync(dataDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nReplication resumes from its checkpoint across a server restart");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
