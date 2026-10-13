// Connections must not stay open forever (see doc/changelog.md, v0.1.8).
// In v0.1.7 an idle or half-sent connection stayed open indefinitely,
// even an unauthenticated one from anywhere on the LAN, and there was no
// cap on how many could be open.
//
// This test owns its server (port 8286) with short limits:
// NYXDB_HEADER_TIMEOUT_SECS=2, NYXDB_IDLE_TIMEOUT_SECS=3,
// NYXDB_MAX_CONNECTIONS=6. It checks that silent and half-sent
// connections are closed after the header timeout, idle keep-alive ones
// after the idle timeout, that connections beyond the cap are refused,
// and that live longpolls (with and without heartbeat) outlive both
// timeouts.
//
// Requires a built binary: NYXDB_BIN (default target/release/nyxdb[.exe]).

const net = require("node:net");
const http = require("node:http");
const { check, finish, sleep, spawnServer } = require("../lib/harness.js");

const PORT = Number(process.env.CONNECTIONS_PORT || 8286);

/// Opens a raw TCP connection, optionally writes `send`, and resolves
/// with how long the server took to close it (null if still open after `ms`).
function closedWithin(send, ms) {
  return new Promise((resolve) => {
    const started = Date.now();
    const socket = net.connect(PORT, "127.0.0.1", () => {
      if (send) socket.write(send);
    });
    socket.on("data", () => {});
    socket.on("error", () => {});
    const timer = setTimeout(() => {
      socket.destroy();
      resolve(null);
    }, ms);
    socket.on("close", () => {
      clearTimeout(timer);
      resolve(Date.now() - started);
    });
  });
}

async function main() {
  const server = await spawnServer({
    port: PORT,
    user: "conn",
    password: "conn",
    env: { NYXDB_HEADER_TIMEOUT_SECS: "2", NYXDB_IDLE_TIMEOUT_SECS: "3", NYXDB_MAX_CONNECTIONS: "6" },
  });
  const auth = server.auth;
  const secs = (ms) => (ms === null ? "still open" : `closed after ${(ms / 1000).toFixed(1)} s`);
  try {
    await fetch(`${server.url}/conn`, { method: "PUT", headers: { Authorization: auth } });

    const silent = await closedWithin(null, 8000);
    check("a connection that sends nothing is closed after the header timeout (2 s)", silent !== null && silent >= 1500 && silent < 5000, secs(silent));

    const half = await closedWithin("GET /conn HTTP/1.1\r\nHost: x\r\nAuthor", 8000);
    check("a half-sent request is closed after the header timeout (2 s)", half !== null && half >= 1500 && half < 5000, secs(half));

    const idle = await closedWithin(`GET / HTTP/1.1\r\nHost: x\r\n\r\n`, 10000);
    check("an idle keep-alive connection is closed after the idle timeout (3 s)", idle !== null && idle >= 2500 && idle < 7000, secs(idle));

    // The cap: six silent connections fill it; a seventh is turned away at once.
    const held = await Promise.all(
      [0, 1, 2, 3, 4, 5].map(() => new Promise((resolve) => { const s = net.connect(PORT, "127.0.0.1", () => resolve(s)); s.on("error", () => {}); }))
    );
    await sleep(200);
    const seventh = await closedWithin(null, 1500);
    held.forEach((s) => s.destroy());
    await sleep(300);
    const after = await fetch(`${server.url}/`).then((r) => r.status, () => "refused");
    check("with NYXDB_MAX_CONNECTIONS=6, a seventh connection is closed at once", seventh !== null && seventh < 1000, secs(seventh));
    check("once connections close, new ones are served again", after === 200, String(after));

    // Live longpolls outlive both timeouts.
    const agent = new http.Agent({ keepAlive: true });
    const longpoll = (query) =>
      new Promise((resolve) => {
        const started = Date.now();
        http
          .get(`${server.url}/conn/_changes?feed=longpoll&${query}`, { agent, headers: { Authorization: auth } }, (r) => {
            let body = "";
            r.on("data", (d) => (body += d));
            r.on("end", () => resolve({ status: r.statusCode, ms: Date.now() - started, body }));
          })
          .on("error", (e) => resolve({ status: e.code, ms: Date.now() - started, body: "" }));
      });
    const { update_seq } = await fetch(`${server.url}/conn`, { headers: { Authorization: auth } }).then((r) => r.json());
    const withHeartbeat = longpoll(`heartbeat=1000&since=${update_seq}`);
    const withoutHeartbeat = longpoll(`timeout=6000&since=${update_seq}`);
    // A continuous feed with no heartbeat: headers sent, then silence past
    // both timeouts. Still a request in progress, so it must stay open.
    const continuous = new Promise((resolve) => {
      let body = "";
      const req = http.get(`${server.url}/conn/_changes?feed=continuous&since=${update_seq}`, { headers: { Authorization: auth } }, (r) => {
        r.on("data", (d) => {
          body += d;
          if (body.includes('"late"')) {
            req.destroy();
            resolve(true);
          }
        });
        r.on("close", () => resolve(body.includes('"late"')));
      });
      req.on("error", () => resolve(false));
    });
    await sleep(7000);
    await fetch(`${server.url}/conn/late`, { method: "PUT", headers: { Authorization: auth, "Content-Type": "application/json" }, body: "{}" });
    const [hb, plain] = await Promise.all([withHeartbeat, withoutHeartbeat]);
    check(
      "a longpoll with heartbeat lasting 7 s (past both timeouts) gets its change",
      hb.status === 200 && hb.ms >= 6500 && hb.body.includes('"late"'),
      `${hb.status} after ${hb.ms} ms: ${hb.body.trim().slice(0, 80)}`
    );
    check("a longpoll without heartbeat runs its full 6 s timeout", plain.status === 200 && plain.ms >= 5500, `${plain.status} after ${plain.ms} ms`);
    check("a continuous feed silent for 7 s stays open and delivers the next change", await continuous, "the feed was closed");
    agent.destroy();
  } finally {
    await server.stop();
  }
  finish("Connections are bounded and time out, live feeds keep working");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
