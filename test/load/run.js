// Load/soak test (plan §6.5): the heaviest real workload this kind of
// server sees is "one device does a fresh initial sync against months
// of history" — approximated here as a large single _bulk_docs batch —
// plus many concurrent long-lived feed=continuous connections (the
// live:true steady state with several devices watching the same db).
// Verifies: a large batch completes in reasonable time, and every
// concurrent continuous subscriber actually receives every change.
//
// Expects the server running at SERVER_URL (default http://127.0.0.1:8085)
// with NYXDB_USER/NYXDB_PASSWORD matching TEST_USER/TEST_PASSWORD.

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const TEST_USER = process.env.TEST_USER || "testuser";
const TEST_PASSWORD = process.env.TEST_PASSWORD || "testpass";
const BULK_SIZE = Number(process.env.LOAD_BULK_SIZE || 2000);
const SUBSCRIBERS = Number(process.env.LOAD_SUBSCRIBERS || 20);
const DB_NAME = "loadtest_" + Date.now();
const AUTH_HEADER = "Basic " + Buffer.from(`${TEST_USER}:${TEST_PASSWORD}`).toString("base64");

function fail(msg) {
  console.error("FAIL:", msg);
  process.exit(1);
}

async function reportServerMemory(label) {
  try {
    const { execSync } = require("child_process");
    const out = execSync('tasklist /FI "IMAGENAME eq nyxdb.exe" /FO CSV /NH').toString();
    const line = out.trim().split("\n")[0];
    if (line && line.includes("nyxdb.exe")) {
      const mem = line.split(",")[4]?.replace(/"/g, "");
      console.log(`  server memory (${label}): ${mem}`);
    }
  } catch {
    // best-effort only; not available outside Windows or if process name differs
  }
}

/// Reads a `feed=continuous` response body, resolving once `target`
/// distinct doc ids have been seen (or rejecting on timeout).
async function subscribeAndCount(url, target, timeoutMs) {
  const seen = new Set();
  const controller = new AbortController();
  const resp = await fetch(url, { headers: { Authorization: AUTH_HEADER }, signal: controller.signal });
  if (!resp.ok) throw new Error(`continuous feed request failed: ${resp.status}`);

  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";

  const donePromise = new Promise(async (resolve, reject) => {
    const timer = setTimeout(() => {
      controller.abort();
      reject(new Error(`timed out with ${seen.size}/${target} changes seen`));
    }, timeoutMs);

    try {
      while (seen.size < target) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let idx;
        while ((idx = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, idx);
          buf = buf.slice(idx + 1);
          if (!line.trim()) continue;
          const row = JSON.parse(line);
          seen.add(row.id);
        }
      }
      clearTimeout(timer);
      controller.abort();
      resolve(seen.size);
    } catch (err) {
      clearTimeout(timer);
      if (seen.size >= target) {
        resolve(seen.size);
      } else {
        reject(err);
      }
    }
  });

  return donePromise;
}

async function main() {
  const createResp = await fetch(`${SERVER_URL}/${DB_NAME}`, {
    method: "PUT",
    headers: { Authorization: AUTH_HEADER },
  });
  if (!createResp.ok) fail(`could not create remote db: ${createResp.status}`);

  await reportServerMemory("before load");

  console.log(`Starting ${SUBSCRIBERS} concurrent feed=continuous subscribers...`);
  const subscriptions = Array.from({ length: SUBSCRIBERS }, () =>
    subscribeAndCount(`${SERVER_URL}/${DB_NAME}/_changes?feed=continuous&since=0`, BULK_SIZE, 30000)
  );
  // Give subscribers a moment to actually establish their connections
  // before the batch write, so this measures "already watching" behavior.
  await new Promise((r) => setTimeout(r, 500));

  const docs = Array.from({ length: BULK_SIZE }, (_, i) => ({ _id: `doc-${i}`, n: i }));
  const start = Date.now();
  const bulkResp = await fetch(`${SERVER_URL}/${DB_NAME}/_bulk_docs`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: AUTH_HEADER },
    body: JSON.stringify({ docs }),
  });
  const elapsedMs = Date.now() - start;
  if (!bulkResp.ok) fail(`_bulk_docs failed: ${bulkResp.status} ${await bulkResp.text()}`);
  const results = await bulkResp.json();
  const errors = results.filter((r) => r.error);
  if (errors.length > 0) fail(`_bulk_docs reported ${errors.length} errors: ${JSON.stringify(errors.slice(0, 3))}`);

  console.log(`_bulk_docs: wrote ${BULK_SIZE} docs in ${elapsedMs}ms (${(BULK_SIZE / (elapsedMs / 1000)).toFixed(0)} docs/sec)`);

  const results2 = await Promise.allSettled(subscriptions);
  const failed = results2.filter((r) => r.status === "rejected");
  if (failed.length > 0) {
    fail(`${failed.length}/${SUBSCRIBERS} subscribers did not see all changes: ${failed[0].reason}`);
  }

  await reportServerMemory("after load");

  console.log(`PASS: all ${SUBSCRIBERS} concurrent continuous subscribers received all ${BULK_SIZE} changes`);
}

main().catch((err) => fail(err.stack || err));
