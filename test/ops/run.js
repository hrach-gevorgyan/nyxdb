// Operational limits (see doc/changelog.md, v0.1.7): a request timeout
// that doesn't cut `_changes` feeds, a cap on requests in flight, request
// tracing, and a cached CORS preflight.
//
// This test owns its server process, on its own port and data dir, with
// small limits: NYXDB_REQUEST_TIMEOUT_SECS=2, NYXDB_MAX_CONCURRENT_REQUESTS=1.
//
// Requires a built binary: NYXDB_BIN (default target/release/nyxdb[.exe]).

const fs = require("node:fs");
const http = require("node:http");
const os = require("node:os");
const path = require("node:path");
const { basicAuth, check, finish, sleep, spawnServer } = require("../lib/harness.js");

const PORT = Number(process.env.OPS_PORT || 8292);
const URL_BASE = `http://127.0.0.1:${PORT}`;
const USER = "ops";
const PASS = "ops-pass";
const AUTH = basicAuth(USER, PASS);
const ORIGIN = "http://app.test";

async function req(method, p, body) {
  const resp = await fetch(`${URL_BASE}${p}`, {
    method,
    headers: { "Content-Type": "application/json", Authorization: AUTH },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  return { status: resp.status, body: await resp.json().catch(() => null) };
}

/// A PUT that announces 100 bytes and sends 10, then stalls. Resolves
/// with the status and when it arrived, or `null` after `giveUpMs`.
function stalledUpload(giveUpMs) {
  return new Promise((resolve) => {
    const started = Date.now();
    const request = http.request(`${URL_BASE}/ops/stalled`, {
      method: "PUT",
      headers: { "Content-Type": "application/json", "Content-Length": 100, Authorization: AUTH },
    });
    const timer = setTimeout(() => {
      request.destroy();
      resolve(null);
    }, giveUpMs);
    request.on("response", (resp) => {
      clearTimeout(timer);
      resp.resume();
      resolve({ status: resp.statusCode, ms: Date.now() - started, at: Date.now() });
      request.destroy();
    });
    request.on("error", () => {});
    request.write('{"a":"1234');
  });
}

async function startServer(logFile) {
  const log = fs.openSync(logFile, "w");
  const server = await spawnServer({
    port: PORT,
    user: USER,
    password: PASS,
    stdio: ["ignore", log, log],
    env: {
      NYXDB_REQUEST_TIMEOUT_SECS: "2",
      NYXDB_MAX_CONCURRENT_REQUESTS: "1",
      NYXDB_CORS_ORIGINS: ORIGIN,
      RUST_LOG: "info,tower_http=debug",
      NO_COLOR: "1",
    },
  });
  return { ...server, log };
}

async function main() {
  const logFile = path.join(os.tmpdir(), `nyxdb-ops-${process.pid}.log`);
  const server = await startServer(logFile);
  try {
    await req("PUT", "/ops");

    const stalled = await stalledUpload(8000);
    check(
      "a stalled upload is answered 408 once the request timeout (2 s) passes",
      stalled?.status === 408 && stalled.ms >= 1500,
      stalled ? `${stalled.status} after ${stalled.ms} ms` : "no response within 8 s"
    );

    // The only slot is held by a stalled upload; a second request waits
    // for it instead of running alongside.
    const holder = stalledUpload(8000);
    await sleep(300);
    const second = await req("GET", "/ops");
    const secondAt = Date.now();
    const held = await holder;
    check(
      "with NYXDB_MAX_CONCURRENT_REQUESTS=1, a second request waits for the first to finish",
      second.status === 200 && held !== null && secondAt >= held.at,
      `second ${second.status}; first ${held ? held.status + " at +" + (held.at - secondAt) + " ms after the second" : "never answered"}`
    );

    // `_changes` is exempt from both: a longpoll may legitimately wait
    // longer than the request timeout, and must not take the only slot.
    const { body: info } = await req("GET", "/ops");
    const started = Date.now();
    const longpoll = req("GET", `/ops/_changes?feed=longpoll&since=${info.update_seq}&timeout=3500`);
    await sleep(300);
    const during = await Promise.race([req("GET", "/ops").then((r) => r.status), sleep(1500).then(() => "blocked")]);
    const lp = await longpoll;
    check(
      "a longpoll outlives the request timeout and doesn't hold a request slot",
      lp.status === 200 && Date.now() - started >= 3000 && during === 200,
      `longpoll ${lp.status} after ${Date.now() - started} ms; request during it: ${during}`
    );

    const preflight = await fetch(`${URL_BASE}/ops/doc`, {
      method: "OPTIONS",
      headers: { Origin: ORIGIN, "Access-Control-Request-Method": "PUT", "Access-Control-Request-Headers": "content-type,authorization" },
    });
    const maxAge = Number(preflight.headers.get("access-control-max-age"));
    check("CORS preflight answers carry Access-Control-Max-Age", maxAge > 0, `max-age ${preflight.headers.get("access-control-max-age")}`);

    await sleep(200);
    const log = fs.readFileSync(logFile, "utf8");
    check(
      "requests are traced (RUST_LOG=tower_http=debug shows each response)",
      /finished processing request/.test(log),
      `log tail: ${log.slice(-300)}`
    );
  } finally {
    await server.stop();
    fs.closeSync(server.log);
    fs.rmSync(logFile, { force: true, maxRetries: 10, retryDelay: 100 });
  }

  finish("Operational limits hold");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
