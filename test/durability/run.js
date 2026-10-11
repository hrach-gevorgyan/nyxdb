// Every acknowledged write must survive the server being killed
// (see doc/changelog.md, v0.1.7).
//
// Offlog stops its embedded NyxDB with TerminateProcess — no shutdown
// hook runs. So an acknowledgement has to mean "on disk", not "in sled's
// write buffer, flushed sometime in the next ~500 ms". And a document's
// tree and its `_changes` entry must land together: if only the tree
// survives, `_revs_diff` says "have it" (so the client never re-sends)
// while `_changes` never lists it (so no other client ever pulls it).
//
// This test owns its server process: it spawns the binary on its own
// port and data dir, writes as fast as it can, hard-kills the process
// right after an acknowledgement, restarts it on the same data dir, and
// checks every acknowledged write. Node's `child.kill()` is
// TerminateProcess on Windows and SIGTERM-without-handler elsewhere; we
// use SIGKILL explicitly so it's a hard kill everywhere.
//
// Requires a built binary: NYXDB_BIN (default target/release/nyxdb[.exe]).

const { spawn } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const BIN =
  process.env.NYXDB_BIN ||
  path.join(__dirname, "..", "..", "target", "release", process.platform === "win32" ? "nyxdb.exe" : "nyxdb");
const PORT = Number(process.env.DURABILITY_PORT || 8299);
const URL_BASE = `http://127.0.0.1:${PORT}`;
const USER = "durability";
const PASS = "durability-pass";
const AUTH = "Basic " + Buffer.from(`${USER}:${PASS}`).toString("base64");
const ROUNDS = Number(process.env.DURABILITY_ROUNDS || 3);
const WRITE_MS = Number(process.env.DURABILITY_WRITE_MS || 1500);
const DB = "durable";

let failures = 0;
function check(name, condition, detail) {
  if (condition) console.log(`PASS: ${name}`);
  else {
    console.error(`FAIL: ${name}${detail ? " — " + detail : ""}`);
    failures++;
  }
}

async function req(method, p, body) {
  const resp = await fetch(`${URL_BASE}/${DB}${p}`, {
    method,
    headers: { "Content-Type": "application/json", Authorization: AUTH },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  return { status: resp.status, body: await resp.json().catch(() => null) };
}

async function startServer(dataDir) {
  const child = spawn(BIN, [], {
    env: { ...process.env, NYXDB_USER: USER, NYXDB_PASSWORD: PASS, NYXDB_DATA: dataDir, NYXDB_ADDR: `127.0.0.1:${PORT}` },
    stdio: "ignore",
  });
  const exited = new Promise((resolve) => child.once("exit", resolve));
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`${URL_BASE}/`)).ok) return { child, exited };
    } catch {}
    await new Promise((r) => setTimeout(r, 50));
  }
  child.kill("SIGKILL");
  throw new Error(`server did not start on port ${PORT}`);
}

async function writeUntilKilled(server, round) {
  const acked = { docs: new Map(), locals: new Map() };
  const deadline = Date.now() + WRITE_MS;
  let n = 0;
  while (Date.now() < deadline) {
    const id = `r${round}-doc${n}`;
    if (n % 3 === 0) {
      // Plain PUT.
      const r = await req("PUT", `/${id}`, { n, round });
      if (r.body?.ok) acked.docs.set(id, r.body.rev);
    } else if (n % 3 === 1) {
      // Replication-style push.
      const h = `r${round}n${n}`.padEnd(32, "0");
      const r = await req("POST", "/_bulk_docs", {
        new_edits: false,
        docs: [{ _id: id, _rev: `1-${h}`, _revisions: { start: 1, ids: [h] }, n }],
      });
      if (Array.isArray(r.body) && r.body.length === 0) acked.docs.set(id, `1-${h}`);
    } else {
      // Replication checkpoint.
      const r = await req("PUT", `/_local/ckpt-r${round}`, { last_seq: n });
      if (r.body?.ok) acked.locals.set(`ckpt-r${round}`, n);
    }
    n++;
  }
  // Kill immediately after the last acknowledgement — nothing gets a
  // chance to flush in the background.
  server.child.kill("SIGKILL");
  await server.exited;
  return acked;
}

async function verify(round, acked) {
  const docIds = [...acked.docs.keys()];
  const lostDocs = [];
  for (const id of docIds) {
    const r = await req("GET", `/${id}`);
    if (r.status !== 200) lostDocs.push(id);
  }
  const diff = await req("POST", "/_revs_diff", Object.fromEntries([...acked.docs].map(([id, rev]) => [id, [rev]])));
  const notHave = Object.keys(diff.body || {});
  const changes = await req("GET", "/_changes?since=0");
  const inChanges = new Set((changes.body?.results || []).map((r) => r.id));
  // A doc the server says it has, but which _changes never lists, is the
  // torn-write case: no client would ever re-send or pull it.
  const torn = docIds.filter((id) => !notHave.includes(id) && !inChanges.has(id));

  const lostLocals = [];
  for (const [id, n] of acked.locals) {
    const r = await req("GET", `/_local/${id}`);
    if (r.body?.last_seq !== n) lostLocals.push(`${id} (want ${n}, got ${r.body?.last_seq})`);
  }

  check(
    `round ${round}: all ${docIds.length} acknowledged docs survive a hard kill`,
    lostDocs.length === 0 && notHave.length === 0,
    `lost ${lostDocs.length}, e.g. ${lostDocs.slice(-5).join(", ")}`
  );
  check(`round ${round}: every surviving doc is listed in _changes`, torn.length === 0, `torn: ${torn.join(", ")}`);
  check(`round ${round}: acknowledged _local checkpoints survive`, lostLocals.length === 0, lostLocals.join(", "));
}

async function main() {
  if (!fs.existsSync(BIN)) throw new Error(`no server binary at ${BIN} (build it, or set NYXDB_BIN)`);
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "nyxdb-durability-"));
  try {
    let server = await startServer(dataDir);
    await fetch(`${URL_BASE}/${DB}`, { method: "PUT", headers: { Authorization: AUTH } });
    for (let round = 1; round <= ROUNDS; round++) {
      const acked = await writeUntilKilled(server, round);
      server = await startServer(dataDir);
      await verify(round, acked);
    }
    server.child.kill("SIGKILL");
    await server.exited;
  } finally {
    fs.rmSync(dataDir, { recursive: true, force: true });
  }

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nEvery acknowledged write survived a hard kill");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
