// Phase 2 acceptance test (plan §8): db.sync({live:true, retry:true}) is
// the actual real-world usage shape (plan §2.2) - two independent
// PouchDB instances both synced live to the server should converge
// without either side polling or restarting.
//
// Expects the server running at SERVER_URL (default http://127.0.0.1:8085).

const PouchDB = require("pouchdb");

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const DB_NAME = "livetest_" + Date.now();

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
  const createResp = await fetch(remoteUrl, { method: "PUT" });
  if (!createResp.ok) fail(`could not create remote db: ${createResp.status}`);

  const deviceA = new PouchDB(DB_NAME + "_a");
  const deviceB = new PouchDB(DB_NAME + "_b");

  const syncA = deviceA.sync(remoteUrl, { live: true, retry: true });
  const syncB = deviceB.sync(remoteUrl, { live: true, retry: true });

  // Give both live syncs a moment to establish their continuous _changes
  // subscriptions before we write.
  await new Promise((r) => setTimeout(r, 500));

  await deviceA.put({ _id: "from-a", text: "hello from device A" });

  const seenOnB = await waitFor(async () => {
    try {
      const doc = await deviceB.get("from-a");
      return doc.text === "hello from device A";
    } catch {
      return false;
    }
  });

  if (!seenOnB) fail("device B never received the doc device A wrote, via live sync");

  await deviceB.put({ _id: "from-b", text: "hello from device B" });

  const seenOnA = await waitFor(async () => {
    try {
      const doc = await deviceA.get("from-b");
      return doc.text === "hello from device B";
    } catch {
      return false;
    }
  });

  if (!seenOnA) fail("device A never received the doc device B wrote, via live sync");

  syncA.cancel();
  syncB.cancel();
  await deviceA.destroy();
  await deviceB.destroy();

  console.log("PASS: two devices converged via live:true sync through the Rust server");
}

main().catch((err) => fail(err.stack || err));
