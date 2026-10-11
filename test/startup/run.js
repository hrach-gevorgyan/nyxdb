// Startup failures must exit with a clear message and a distinct exit
// code, not a Rust panic (see doc/changelog.md, v0.1.7).
//
// Offlog launches NyxDB as a sidecar and needs to tell the user *why* it
// didn't start. A panic prints "thread 'main' panicked at ..." plus a
// backtrace hint and always exits 101, so a bad setting, a port that's
// already taken, and an unusable data directory all look identical.
//
// Exit codes: 2 = bad configuration, 3 = data directory unusable,
// 4 = can't listen on the address.
//
// This test spawns the binary itself: NYXDB_BIN (default
// target/release/nyxdb[.exe]).

const { spawn } = require("node:child_process");
const fs = require("node:fs");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { BIN, check, finish } = require("../lib/harness.js");

const PORT = Number(process.env.STARTUP_PORT || 8297);

function run(env) {
  return new Promise((resolve) => {
    const child = spawn(BIN, [], {
      env: { ...process.env, NYXDB_USER: "s", NYXDB_PASSWORD: "s", ...env },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let output = "";
    child.stdout.on("data", (d) => (output += d));
    child.stderr.on("data", (d) => (output += d));
    const timer = setTimeout(() => child.kill("SIGKILL"), 10000);
    child.on("exit", (code) => {
      clearTimeout(timer);
      resolve({ code, output });
    });
  });
}

const panicked = (output) => /panicked at|RUST_BACKTRACE/.test(output);

async function main() {
  if (!fs.existsSync(BIN)) throw new Error(`no server binary at ${BIN} (build it, or set NYXDB_BIN)`);
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "nyxdb-startup-"));
  try {
    const badLevel = await run({ NYXDB_DATA: path.join(tmp, "a"), NYXDB_ADDR: `127.0.0.1:${PORT}`, NYXDB_COMPRESSION_LEVEL: "loud" });
    check(
      "an invalid NYXDB_COMPRESSION_LEVEL exits 2 with a message naming the setting, no panic",
      badLevel.code === 2 && /NYXDB_COMPRESSION_LEVEL/.test(badLevel.output) && !panicked(badLevel.output),
      `exit ${badLevel.code}: ${badLevel.output.trim().slice(-200)}`
    );

    const blocker = net.createServer().listen(PORT, "127.0.0.1");
    await new Promise((r) => blocker.once("listening", r));
    const taken = await run({ NYXDB_DATA: path.join(tmp, "b"), NYXDB_ADDR: `127.0.0.1:${PORT}` });
    blocker.close();
    check(
      "an address already in use exits 4 with a message naming the address, no panic",
      taken.code === 4 && taken.output.includes(`127.0.0.1:${PORT}`) && !panicked(taken.output),
      `exit ${taken.code}: ${taken.output.trim().slice(-200)}`
    );

    const notADir = path.join(tmp, "plain-file");
    fs.writeFileSync(notADir, "not a directory");
    const badData = await run({ NYXDB_DATA: notADir, NYXDB_ADDR: `127.0.0.1:${PORT}` });
    check(
      "an unusable data directory exits 3 with a message naming the path, no panic",
      badData.code === 3 && badData.output.includes("plain-file") && !panicked(badData.output),
      `exit ${badData.code}: ${badData.output.trim().slice(-200)}`
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
