// A deletion must reach a replica that already has the doc — as a
// deletion, not a resurrected or conflicting copy (see doc/changelog.md,
// v0.1.7).
//
// Two live-syncing PouchDB clients both hold an edited doc. Then:
// - one client deletes it: the other must end up with it deleted, with
//   the full history and no conflict;
// - the server deletes another one (`DELETE /{db}/{id}`): both clients
//   must end up with it deleted.
//
// Expects the server running at SERVER_URL (default http://127.0.0.1:8085)
// with NYXDB_USER/NYXDB_PASSWORD matching TEST_USER/TEST_PASSWORD.

const PouchDB = require("./pouchdb");

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const TEST_USER = process.env.TEST_USER || "testuser";
const TEST_PASSWORD = process.env.TEST_PASSWORD || "testpass";
const DB_NAME = "deltest_" + Date.now();
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

const revOf = async (db, id) => (await db.get(id).catch(() => null))?._rev;

/// The doc's leaves on `db`: exactly one, a tombstone, with `generations`
/// revisions of history.
async function assertDeletedCleanly(db, label, id, generations) {
  const missing = await db.get(id).then(() => null, (err) => err);
  if (missing?.status !== 404) throw new Error(`${label}: ${id} is still readable after the deletion`);
  const leaves = await db.get(id, { open_revs: "all", revs: true });
  if (leaves.length !== 1 || !leaves[0].ok?._deleted) {
    throw new Error(`${label}: ${id} should have one deleted leaf, has ${JSON.stringify(leaves.map((l) => l.ok?._rev + (l.ok?._deleted ? " (deleted)" : "")))}`);
  }
  if (leaves[0].ok._revisions.ids.length !== generations) {
    throw new Error(`${label}: ${id} lost history: ${JSON.stringify(leaves[0].ok._revisions)}`);
  }
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
    // Both devices hold rev 2 of both docs.
    for (const id of ["by-client", "by-server"]) {
      const created = await deviceA.put({ _id: id, v: 1 });
      const edited = await deviceA.put({ _id: id, _rev: created.rev, v: 2 });
      if (!(await waitFor(async () => (await revOf(deviceB, id)) === edited.rev))) {
        throw new Error(`device B never received ${id} rev 2`);
      }
    }

    const doc = await deviceA.get("by-client");
    await deviceA.remove(doc);
    if (!(await waitFor(async () => (await revOf(deviceB, "by-client")) === undefined))) {
      throw new Error("device B still has by-client after device A deleted it");
    }
    await new Promise((r) => setTimeout(r, 500));
    await assertDeletedCleanly(deviceB, "device B", "by-client", 3);
    await assertDeletedCleanly(deviceA, "device A", "by-client", 3);

    const current = await deviceA.get("by-server");
    const del = await fetch(`${remoteUrl}/by-server?rev=${current._rev}`, { method: "DELETE", headers: { Authorization: AUTH_HEADER } });
    if (!del.ok) throw new Error(`server DELETE failed: ${del.status}`);
    for (const [label, db] of [["device A", deviceA], ["device B", deviceB]]) {
      if (!(await waitFor(async () => (await revOf(db, "by-server")) === undefined))) {
        throw new Error(`${label} still has by-server after the server deleted it`);
      }
    }
    await new Promise((r) => setTimeout(r, 500));
    await assertDeletedCleanly(deviceA, "device A", "by-server", 3);
    await assertDeletedCleanly(deviceB, "device B", "by-server", 3);
  } catch (err) {
    failure = err;
  }

  syncA.cancel();
  syncB.cancel();
  await deviceA.destroy();
  await deviceB.destroy();
  await fetch(remoteUrl, { method: "DELETE", headers: { Authorization: AUTH_HEADER } });

  if (failure) fail(failure.message);
  console.log("PASS: deletions (client- and server-side) reach replicas that already have the doc, cleanly");
}

main().catch((err) => fail(err.stack || err));
