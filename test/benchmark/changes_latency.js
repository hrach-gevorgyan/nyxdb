// Measures true insert-to-subscriber-notification latency for
// feed=continuous (the actual metric suggestions.md Scenario 3 targets,
// as opposed to a full HTTP round-trip for a normal _changes request).
// One already-connected continuous subscriber; we time from just before
// each PUT request resolves to when that change line arrives on the
// subscriber's stream.
//
// Requires: OUR_URL/OUR_USER/OUR_PASSWORD (defaults: 127.0.0.1:8085, testuser/testpass)

const OUR_URL = process.env.OUR_URL || "http://127.0.0.1:8085";
const OUR_USER = process.env.OUR_USER || "testuser";
const OUR_PASSWORD = process.env.OUR_PASSWORD || "testpass";
const SAMPLE_COUNT = Number(process.env.LATENCY_SAMPLES || 100);
const DB_NAME = "latencytest_" + Date.now();
const AUTH = "Basic " + Buffer.from(`${OUR_USER}:${OUR_PASSWORD}`).toString("base64");

async function req(method, path, body) {
  const resp = await fetch(`${OUR_URL}${path}`, {
    method,
    headers: { "Content-Type": "application/json", Authorization: AUTH },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  return { status: resp.status, body: await resp.json().catch(() => null) };
}

async function main() {
  await req("PUT", `/${DB_NAME}`);

  // Open one continuous subscriber and keep it running for the whole test.
  const arrivalTimes = new Map(); // doc id -> Date.now() when the line arrived
  const controller = new AbortController();
  const resp = await fetch(`${OUR_URL}/${DB_NAME}/_changes?feed=continuous&since=0`, {
    headers: { Authorization: AUTH },
    signal: controller.signal,
  });
  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  (async () => {
    try {
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let idx;
        while ((idx = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, idx);
          buf = buf.slice(idx + 1);
          if (!line.trim()) continue;
          const row = JSON.parse(line);
          if (!arrivalTimes.has(row.id)) arrivalTimes.set(row.id, Date.now());
        }
      }
    } catch (err) {
      if (!controller.signal.aborted) console.error("reader loop died unexpectedly:", err);
      // aborted at the end, expected
    }
  })();

  await new Promise((r) => setTimeout(r, 300)); // let the subscription establish

  const latencies = [];
  for (let i = 0; i < SAMPLE_COUNT; i++) {
    const id = `doc-${i}`;
    const r = await req("PUT", `/${DB_NAME}/${id}`, { n: i });
    if (r.status !== 201 && r.status !== 200) throw new Error(`write failed: ${r.status}`);
    // Measured AFTER the write's own HTTP round-trip completes, isolating
    // "server committed it -> subscriber's stream received it" instead of
    // including the write's own network latency.
    const sendTime = Date.now();

    // Wait for this doc's change to show up on the continuous stream.
    const deadline = Date.now() + 5000;
    while (!arrivalTimes.has(id) && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 1));
    }
    if (!arrivalTimes.has(id)) throw new Error(`doc ${id} never arrived on continuous feed`);
    latencies.push(arrivalTimes.get(id) - sendTime);
  }

  controller.abort();
  await req("DELETE", `/${DB_NAME}`);

  latencies.sort((a, b) => a - b);
  const avg = latencies.reduce((a, b) => a + b, 0) / latencies.length;
  const p50 = latencies[Math.floor(latencies.length * 0.5)];
  const p95 = latencies[Math.floor(latencies.length * 0.95)];
  const max = latencies[latencies.length - 1];

  console.log(`_changes continuous propagation latency (write ACKed -> subscriber's stream receives it), n=${SAMPLE_COUNT}`);
  console.log(`  avg: ${avg.toFixed(2)}ms   p50: ${p50}ms   p95: ${p95}ms   max: ${max}ms`);
  console.log(`  This isolates broadcast-channel + delivery time, excluding the write's own`);
  console.log(`  HTTP round-trip (measured after the PUT response is already received).`);
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
