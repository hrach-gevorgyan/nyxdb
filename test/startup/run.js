// Startup failures must exit with a clear message and a distinct exit
// code, not a Rust panic (see doc/changelog.md, v0.1.7 and v0.1.8).
//
// Offlog launches NyxDB as a sidecar and needs to tell the user *why* it
// didn't start. A panic prints "thread 'main' panicked at ..." plus a
// backtrace hint and always exits 101, so a bad setting, a port that's
// already taken, and an unusable data directory all look identical.
//
// Exit codes: 2 = bad configuration, 3 = the data directory or its store
// can't be opened or is damaged (Offlog then moves the folder aside and
// starts fresh), 4 = can't listen on the address. Each failure prints one
// line on stderr saying why.
//
// This test spawns the binary itself: NYXDB_BIN (default
// target/release/nyxdb[.exe]).

const { spawn } = require("node:child_process");
const crypto = require("node:crypto");
const fs = require("node:fs");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { BIN, check, finish, sleep } = require("../lib/harness.js");

const PORT = Number(process.env.STARTUP_PORT || 8297);

/// Runs the binary with `env` (a null value removes the variable). Resolves
/// when it exits, or — with `untilListening` — once it is listening, after
/// a hard kill.
function run(env, { untilListening = false } = {}) {
  return new Promise((resolve) => {
    const fullEnv = { ...process.env, NYXDB_USER: "s", NYXDB_PASSWORD: "s", ...env };
    for (const [k, v] of Object.entries(fullEnv)) if (v === null) delete fullEnv[k];
    const child = spawn(BIN, [], { env: fullEnv, stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => {
      stdout += d;
      if (untilListening && /listening on/.test(stdout)) child.kill("SIGKILL");
    });
    child.stderr.on("data", (d) => (stderr += d));
    const timer = setTimeout(() => child.kill("SIGKILL"), 10000);
    child.on("exit", (code) => {
      clearTimeout(timer);
      resolve({ code, stdout, stderr, output: stdout + stderr, listened: /listening on/.test(stdout) });
    });
  });
}

const panicked = (output) => /panicked at|RUST_BACKTRACE/.test(output);
const oneLine = (stderr) => stderr.trim().split("\n").length === 1;

async function main() {
  if (!fs.existsSync(BIN)) throw new Error(`no server binary at ${BIN} (build it, or set NYXDB_BIN)`);
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "nyxdb-startup-"));
  const addr = `127.0.0.1:${PORT}`;
  try {
    const badLevel = await run({ NYXDB_DATA: path.join(tmp, "a"), NYXDB_ADDR: addr, NYXDB_COMPRESSION_LEVEL: "loud" });
    check(
      "an invalid NYXDB_COMPRESSION_LEVEL exits 2 with a message naming the setting, no panic",
      badLevel.code === 2 && /NYXDB_COMPRESSION_LEVEL/.test(badLevel.output) && !panicked(badLevel.output),
      `exit ${badLevel.code}: ${badLevel.output.trim().slice(-200)}`
    );

    const blocker = net.createServer().listen(PORT, "127.0.0.1");
    await new Promise((r) => blocker.once("listening", r));
    const taken = await run({ NYXDB_DATA: path.join(tmp, "b"), NYXDB_ADDR: addr });
    blocker.close();
    check(
      "an address already in use exits 4 with a message naming the address, no panic",
      taken.code === 4 && taken.output.includes(addr) && !panicked(taken.output),
      `exit ${taken.code}: ${taken.output.trim().slice(-200)}`
    );

    const notADir = path.join(tmp, "plain-file");
    fs.writeFileSync(notADir, "not a directory");
    const badData = await run({ NYXDB_DATA: notADir, NYXDB_ADDR: addr });
    check(
      "an unusable data directory exits 3 with a message naming the path, no panic",
      badData.code === 3 && badData.output.includes("plain-file") && !panicked(badData.output),
      `exit ${badData.code}: ${badData.output.trim().slice(-200)}`
    );

    // credentials.json: written atomically, and never silently replaced.
    const credsDir = path.join(tmp, "creds");
    const fresh = await run({ NYXDB_DATA: credsDir, NYXDB_ADDR: addr, NYXDB_USER: null, NYXDB_PASSWORD: null }, { untilListening: true });
    const credsFile = path.join(credsDir, "credentials.json");
    let creds = null;
    try {
      creds = JSON.parse(fs.readFileSync(credsFile, "utf8"));
    } catch {}
    const strays = fs.readdirSync(credsDir).filter((f) => f.startsWith("credentials") && f !== "credentials.json");
    check(
      "a first start writes a valid credentials.json and leaves no temp file",
      fresh.listened && creds?.username && creds?.password && strays.length === 0,
      `listened ${fresh.listened}; ${JSON.stringify(creds)}; strays ${strays.join(", ")}`
    );
    const garbled = '{"username": "admin", "passw';
    fs.writeFileSync(credsFile, garbled);
    const unreadable = await run({ NYXDB_DATA: credsDir, NYXDB_ADDR: addr, NYXDB_USER: null, NYXDB_PASSWORD: null });
    check(
      "an unparseable credentials.json exits 3 with one stderr line naming it, and is left as it was",
      unreadable.code === 3 && oneLine(unreadable.stderr) && /credentials\.json/.test(unreadable.stderr) && fs.readFileSync(credsFile, "utf8") === garbled,
      `exit ${unreadable.code}; stderr ${JSON.stringify(unreadable.stderr.trim().slice(-200))}; file now ${JSON.stringify(fs.readFileSync(credsFile, "utf8").slice(0, 60))}`
    );

    // A damaged store: exit 3, never a fresh empty store in its place.
    const storeDir = path.join(tmp, "store");
    const first = await run({ NYXDB_DATA: storeDir, NYXDB_ADDR: addr }, { untilListening: true });
    if (!first.listened) throw new Error(`server didn't start: ${first.output}`);
    // Put some data in it through a normal run.
    {
      const child = spawn(BIN, [], { env: { ...process.env, NYXDB_USER: "s", NYXDB_PASSWORD: "s", NYXDB_DATA: storeDir, NYXDB_ADDR: addr }, stdio: "ignore" });
      for (let i = 0; i < 100; i++) {
        try {
          if ((await fetch(`http://${addr}/`)).ok) break;
        } catch {}
        await sleep(50);
      }
      const auth = { Authorization: "Basic " + Buffer.from("s:s").toString("base64") };
      await fetch(`http://${addr}/keep`, { method: "PUT", headers: auth });
      await fetch(`http://${addr}/keep/doc`, { method: "PUT", headers: { ...auth, "Content-Type": "application/json" }, body: "{}" });
      child.kill("SIGKILL");
      await new Promise((r) => child.once("exit", r));
    }
    // Kinds of damage v0.1.7 accepted silently, starting an empty store
    // (and overwriting the damaged file) instead of failing.
    const damages = {
      "zero-filled": (f) => fs.writeFileSync(f, Buffer.alloc(fs.statSync(f).size)),
      emptied: (f) => fs.writeFileSync(f, ""),
      "replaced by 4 KB of garbage": (f) => fs.writeFileSync(f, crypto.randomBytes(4096)),
      "garbage over its first 4 KB": (f) => {
        const bytes = fs.readFileSync(f);
        crypto.randomBytes(4096).copy(bytes, 0);
        fs.writeFileSync(f, bytes);
      },
    };
    for (const [kind, damage] of Object.entries(damages)) {
      const copy = path.join(tmp, `damaged-${kind.replace(/\W+/g, "-")}`);
      fs.cpSync(storeDir, copy, { recursive: true });
      damage(path.join(copy, "db"));
      // As after a hard kill before sled wrote its first snapshot (how
      // Offlog stops NyxDB): with no snapshot to check against, v0.1.7
      // accepted any damage silently.
      for (const f of fs.readdirSync(copy)) if (f.startsWith("snap.")) fs.rmSync(path.join(copy, f));
      const damaged = await run({ NYXDB_DATA: copy, NYXDB_ADDR: addr });
      check(
        `a store whose db file is ${kind} exits 3 with one stderr line, instead of starting empty`,
        damaged.code === 3 && oneLine(damaged.stderr) && /damaged|can't open/.test(damaged.stderr) && !damaged.listened && !panicked(damaged.output),
        `exit ${damaged.code}, listened ${damaged.listened}; stderr ${JSON.stringify(damaged.stderr.trim().slice(-300))}`
      );
    }
    const badConf = path.join(tmp, "conf-store");
    fs.cpSync(storeDir, badConf, { recursive: true });
    fs.writeFileSync(path.join(badConf, "conf"), crypto.randomBytes(64));
    const confDamaged = await run({ NYXDB_DATA: badConf, NYXDB_ADDR: addr });
    check(
      "a damaged store config exits 3 with one stderr line, no panic",
      confDamaged.code === 3 && oneLine(confDamaged.stderr) && !panicked(confDamaged.output),
      `exit ${confDamaged.code}; stderr ${JSON.stringify(confDamaged.stderr.trim().slice(-300))}`
    );
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }

  finish("Startup failures exit cleanly with distinct codes");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
