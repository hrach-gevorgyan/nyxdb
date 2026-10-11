// A `_changes` follower must never permanently miss a change, no matter
// how writes interleave with its reads (see doc/changelog.md, v0.1.7).
//
// Two followers run while many writers hammer the same database:
//
// - a polling follower on the normal feed: `_changes?since=X`, then
//   `X = last_seq`, repeat. If the server reports a `last_seq` higher
//   than the changes it actually returned (e.g. by reading the current
//   seq *after* collecting results, with a write landing in between),
//   that write's seq is now behind the follower's checkpoint and is
//   skipped forever.
// - a `feed=continuous` follower. It drops events with seq <= the last
//   one it sent, so if seqs ever become visible or get broadcast out of
//   order, an earlier write that lands late is skipped forever.
//
// After the writers finish, both followers get a final catch-up; each
// must have seen every document written.
//
// No dependencies (native fetch).
// Requires: SERVER_URL/OUR_USER/OUR_PASSWORD (defaults: 127.0.0.1:8085, testuser/testpass)

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const OUR_USER = process.env.OUR_USER || "testuser";
const OUR_PASSWORD = process.env.OUR_PASSWORD || "testpass";
const AUTH = "Basic " + Buffer.from(`${OUR_USER}:${OUR_PASSWORD}`).toString("base64");
const WRITERS = Number(process.env.FOLLOW_WRITERS || 16);
const DOCS_PER_WRITER = Number(process.env.FOLLOW_DOCS || 60);

let failures = 0;

function check(name, condition, detail) {
  if (condition) {
    console.log(`PASS: ${name}`);
  } else {
    console.error(`FAIL: ${name}${detail ? " — " + detail : ""}`);
    failures++;
  }
}

async function req(db, method, path, body) {
  const resp = await fetch(`${SERVER_URL}/${db}${path}`, {
    method,
    headers: { "Content-Type": "application/json", Authorization: AUTH },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  return { status: resp.status, body: await resp.json().catch(() => null) };
}

function continuousFollower(db, seen) {
  const controller = new AbortController();
  const done = (async () => {
    const resp = await fetch(`${SERVER_URL}/${db}/_changes?feed=continuous&since=0`, {
      headers: { Authorization: AUTH },
      signal: controller.signal,
    });
    const decoder = new TextDecoder();
    let buffer = "";
    try {
      for await (const chunk of resp.body) {
        buffer += decoder.decode(chunk, { stream: true });
        let nl;
        while ((nl = buffer.indexOf("\n")) >= 0) {
          const line = buffer.slice(0, nl).trim();
          buffer = buffer.slice(nl + 1);
          if (line) seen.add(JSON.parse(line).id);
        }
      }
    } catch (err) {
      if (err.name !== "AbortError") throw err;
    }
  })();
  // Resolve to the stream's error (or null) instead of rejecting, so a
  // dropped connection is reported as a failed check, never an unhandled
  // rejection that kills the process with no FAIL line.
  return { stop: () => controller.abort(), done: done.then(() => null, (err) => err) };
}

async function main() {
  const db = "follow_" + Date.now();
  const created = await req(db, "PUT", "");
  if (created.status !== 200 && created.status !== 201) throw new Error(`could not create ${db}: ${created.status}`);

  const expected = new Set();
  for (let w = 0; w < WRITERS; w++) for (let i = 0; i < DOCS_PER_WRITER; i++) expected.add(`w${w}-d${i}`);

  const polled = new Set();
  const streamed = new Set();
  const stream = continuousFollower(db, streamed);
  let since = 0;
  let writing = true;
  let polls = 0;

  const poll = async () => {
    const r = await req(db, "GET", `/_changes?since=${since}`);
    for (const row of r.body.results) polled.add(row.id);
    since = r.body.last_seq;
    polls++;
  };

  try {
    const poller = (async () => {
      while (writing) await poll();
    })();
    await Promise.all(
      Array.from({ length: WRITERS }, async (_, w) => {
        for (let i = 0; i < DOCS_PER_WRITER; i++) await req(db, "PUT", `/w${w}-d${i}`, { w, i });
      })
    );
    writing = false;
    await poller;
    await poll(); // final catch-up from the follower's own checkpoint

    // Give the continuous stream a moment to drain, then stop it.
    const deadline = Date.now() + 5000;
    while (streamed.size < expected.size && Date.now() < deadline) await new Promise((r) => setTimeout(r, 100));
    stream.stop();
    const streamError = await stream.done;
    check("continuous follower's connection stayed up", streamError === null, String(streamError?.cause || streamError));

    const missingPolled = [...expected].filter((id) => !polled.has(id));
    const missingStreamed = [...expected].filter((id) => !streamed.has(id));
    check(
      `polling follower (${polls} polls by last_seq) saw all ${expected.size} docs written by ${WRITERS} parallel writers`,
      missingPolled.length === 0,
      `missed ${missingPolled.length}, e.g. ${missingPolled.slice(0, 5).join(", ")}`
    );
    check(
      `continuous follower saw all ${expected.size} docs`,
      missingStreamed.length === 0,
      `missed ${missingStreamed.length}, e.g. ${missingStreamed.slice(0, 5).join(", ")}`
    );
  } finally {
    stream.stop();
    await req(db, "DELETE", "");
  }

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nNo change was skipped by either follower");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
