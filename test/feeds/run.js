// `_changes` longpoll/continuous behaviour on the wire: `heartbeat`,
// `timeout`, and waking promptly on a write (see doc/changelog.md,
// v0.1.7).
//
// PouchDB's live sync uses `feed=longpoll&heartbeat=10000` and gives the
// request its own client-side timeout of heartbeat + 5 s. A server that
// ignores `heartbeat` and holds the request for its 60 s default gets
// aborted by the client every ~15 s. CouchDB sends a newline every
// `heartbeat` ms while it waits, which keeps the request (and any proxy
// in between) alive; JSON parsers skip the leading whitespace.
//
// No dependencies (native fetch).
// Requires: SERVER_URL/OUR_USER/OUR_PASSWORD (defaults: 127.0.0.1:8085, testuser/testpass)

const { SERVER_URL, OUR_USER, OUR_PASSWORD, AUTH, check, finish } = require("../lib/harness.js");
async function req(db, method, path, body) {
  const resp = await fetch(`${SERVER_URL}/${db}${path}`, {
    method,
    headers: { "Content-Type": "application/json", Authorization: AUTH },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  return { status: resp.status, body: await resp.json().catch(() => null) };
}

// Reads a streamed response for up to `ms`, returning what arrived and
// whether the stream ended on its own.
async function readFor(db, path, ms) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), ms);
  const started = Date.now();
  let text = "";
  let ended = false;
  try {
    const resp = await fetch(`${SERVER_URL}/${db}${path}`, { headers: { Authorization: AUTH }, signal: controller.signal });
    const decoder = new TextDecoder();
    for await (const chunk of resp.body) text += decoder.decode(chunk, { stream: true });
    ended = true;
  } catch (err) {
    if (err.name !== "AbortError") throw err;
  } finally {
    clearTimeout(timer);
  }
  return { text, ended, elapsed: Date.now() - started };
}

async function main() {
  const db = "feeds_" + Date.now();
  await req(db, "PUT", "");
  try {
    await req(db, "PUT", "/seed", { v: 1 });
    const since = (await req(db, "GET", "/_changes")).body.last_seq;

    const lpHeartbeat = await readFor(db, `/_changes?feed=longpoll&since=${since}&heartbeat=300`, 1500);
    check(
      "longpoll with heartbeat=300 sends newlines while it waits",
      !lpHeartbeat.ended && /\n/.test(lpHeartbeat.text),
      `ended=${lpHeartbeat.ended} received=${JSON.stringify(lpHeartbeat.text)}`
    );

    const contHeartbeat = await readFor(db, `/_changes?feed=continuous&since=${since}&heartbeat=300`, 1500);
    check(
      "continuous with heartbeat=300 sends newlines while idle",
      /\n/.test(contHeartbeat.text) && contHeartbeat.text.trim() === "",
      `received=${JSON.stringify(contHeartbeat.text)}`
    );

    // With a heartbeat, the eventual change still arrives as one JSON
    // body (after the keep-alive newlines).
    const pending = readFor(db, `/_changes?feed=longpoll&since=${since}&heartbeat=200`, 4000);
    await new Promise((r) => setTimeout(r, 700));
    await req(db, "PUT", "/after-heartbeats", { v: 1 });
    const woke = await pending;
    let parsed = null;
    try {
      parsed = JSON.parse(woke.text);
    } catch {}
    check(
      "longpoll with heartbeat ends with a parseable body containing the new change",
      woke.ended && parsed?.results?.some((r) => r.id === "after-heartbeats") && woke.text.startsWith("\n"),
      `ended=${woke.ended} text=${JSON.stringify(woke.text.slice(0, 200))}`
    );

    const since2 = parsed?.last_seq ?? since;
    const lpTimeout = await readFor(db, `/_changes?feed=longpoll&since=${since2}&timeout=300`, 3000);
    let timedOut = null;
    try {
      timedOut = JSON.parse(lpTimeout.text);
    } catch {}
    check(
      "longpoll timeout=300 returns an empty result about 300 ms later",
      lpTimeout.ended && lpTimeout.elapsed < 1500 && timedOut?.results?.length === 0 && timedOut.last_seq === since2,
      `ended=${lpTimeout.ended} elapsed=${lpTimeout.elapsed}ms body=${lpTimeout.text}`
    );

    const prompt = readFor(db, `/_changes?feed=longpoll&since=${since2}`, 5000);
    await new Promise((r) => setTimeout(r, 200));
    await req(db, "PUT", "/prompt", { v: 1 });
    const promptResult = await prompt;
    check(
      "longpoll without a timeout returns promptly when a write lands",
      promptResult.ended && promptResult.elapsed < 2000 && promptResult.text.includes('"prompt"'),
      `ended=${promptResult.ended} elapsed=${promptResult.elapsed}ms`
    );
  } finally {
    await req(db, "DELETE", "");
  }

  finish("longpoll/continuous heartbeat and timeout behave like CouchDB");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
