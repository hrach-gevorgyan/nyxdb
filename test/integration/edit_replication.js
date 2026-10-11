// Edits to an already-replicated document must not turn into phantom
// conflicts on the other replica (see doc/changelog.md, v0.1.6).
//
// A first full sync only ever moves rev-1 docs, which have no history to
// lose — that's why live_sync.js alone never caught this. The failure
// needs a doc that already exists on both replicas, then gets edited on
// one: the pulling side fetches the new revision via `_bulk_get` with
// `revs=true`, and if the server leaves out `_revisions`, PouchDB stores
// the new rev (new_edits:false) as a fresh root next to the copy it
// already had — a conflict nobody created.
//
// Expects the server running at SERVER_URL (default http://127.0.0.1:8085)
// with NYXDB_USER/NYXDB_PASSWORD matching TEST_USER/TEST_PASSWORD.

const PouchDB = require("pouchdb");

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const TEST_USER = process.env.TEST_USER || "testuser";
const TEST_PASSWORD = process.env.TEST_PASSWORD || "testpass";
const DB_NAME = "edittest_" + Date.now();
const AUTH_HEADER = "Basic " + Buffer.from(`${TEST_USER}:${TEST_PASSWORD}`).toString("base64");
const AUTH_OPT = { auth: { username: TEST_USER, password: TEST_PASSWORD } };

function fail(msg) {
  console.error("FAIL:", msg);
  process.exit(1);
}

async function waitFor(fn, timeoutMs = 8000) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    if (await fn()) return true;
    await new Promise((r) => setTimeout(r, 100));
  }
  return false;
}

async function main() {
  const remoteUrl = `${SERVER_URL}/${DB_NAME}`;
  const createResp = await fetch(remoteUrl, { method: "PUT", headers: { Authorization: AUTH_HEADER } });
  if (!createResp.ok) fail(`could not create remote db: ${createResp.status}`);

  const deviceA = new PouchDB(DB_NAME + "_a");
  const deviceB = new PouchDB(DB_NAME + "_b");
  const syncA = deviceA.sync(remoteUrl, { live: true, retry: true, ...AUTH_OPT });
  const syncB = deviceB.sync(remoteUrl, { live: true, retry: true, ...AUTH_OPT });
  await new Promise((r) => setTimeout(r, 500));

  let failure = null;
  try {
    const created = await deviceA.put({ _id: "doc", text: "v1" });
    if (!(await waitFor(async () => (await deviceB.get("doc").catch(() => null))?._rev === created.rev))) {
      throw new Error("device B never received the original doc");
    }

    // Two separate edits on A, each given time to replicate on its own.
    let rev = created.rev;
    for (const text of ["v2", "v3"]) {
      rev = (await deviceA.put({ _id: "doc", _rev: rev, text })).rev;
      const target = rev;
      if (!(await waitFor(async () => (await deviceB.get("doc").catch(() => null))?._rev === target))) {
        throw new Error(`device B never received edit ${text} (${target})`);
      }
      await new Promise((r) => setTimeout(r, 500));
    }

    const onB = await deviceB.get("doc", { conflicts: true, revs: true });
    if (onB._conflicts && onB._conflicts.length) {
      throw new Error(`device B has phantom conflicts ${JSON.stringify(onB._conflicts)} after edits made only on A`);
    }
    if (onB._revisions.ids.length !== 3) {
      throw new Error(`device B lost the edit history: _revisions=${JSON.stringify(onB._revisions)}`);
    }
  } catch (err) {
    failure = err;
  }

  syncA.cancel();
  syncB.cancel();
  await deviceA.destroy();
  await deviceB.destroy();
  await fetch(remoteUrl, { method: "DELETE", headers: { Authorization: AUTH_HEADER } });

  if (failure) fail(failure.message);
  console.log("PASS: editing an already-replicated doc leaves the other replica with full history and no conflicts");
}

main().catch((err) => fail(err.stack || err));
