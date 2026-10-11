// Shared plumbing for the test suites: where the server is, how to
// authenticate, PASS/FAIL reporting, and spawning a private server.
//
// Environment (a suite that spawns its own server ignores the first three):
//   SERVER_URL                 default http://127.0.0.1:8085
//   OUR_USER / OUR_PASSWORD    default testuser / testpass
//   TEST_USER / TEST_PASSWORD  older names for the same two; either works
//   NYXDB_BIN                  server binary for spawning suites
//                              (default target/release/nyxdb[.exe])

const { spawn } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const USER = process.env.OUR_USER || process.env.TEST_USER || "testuser";
const PASSWORD = process.env.OUR_PASSWORD || process.env.TEST_PASSWORD || "testpass";
const basicAuth = (user, password) => "Basic " + Buffer.from(`${user}:${password}`).toString("base64");
const AUTH = basicAuth(USER, PASSWORD);

const BIN =
  process.env.NYXDB_BIN ||
  path.join(__dirname, "..", "..", "target", "release", process.platform === "win32" ? "nyxdb.exe" : "nyxdb");

let failureCount = 0;

/// Prints PASS/FAIL for one check; `failures()` counts the FAILs.
function check(name, condition, detail) {
  if (condition) console.log(`PASS: ${name}`);
  else {
    console.error(`FAIL: ${name}${detail ? " — " + detail : ""}`);
    failureCount++;
  }
}

const failures = () => failureCount;

/// Ends a suite: exit 1 if any check failed, else print `summary`.
function finish(summary) {
  if (failureCount > 0) {
    console.error(`\n${failureCount} check(s) failed`);
    process.exit(1);
  }
  console.log(`\n${summary}`);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/// Removes a directory, retrying while Windows still holds files in it.
function removeDir(dir) {
  fs.rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
}

/// Resident memory of process `pid` in MB (working set on Windows).
function residentMB(pid) {
  if (process.platform === "win32") {
    const out = require("node:child_process").execSync(`powershell -NoProfile -Command "(Get-Process -Id ${pid}).WorkingSet64"`);
    return Number(out.toString()) / 1048576;
  }
  return Number(fs.readFileSync(`/proc/${pid}/status`, "utf8").match(/VmRSS:\s+(\d+)/)[1]) / 1024;
}

/// Starts the server binary on `port` with a fresh temp data dir and waits
/// until `GET /` answers. `env` adds/overrides variables; `stdio` is
/// passed to spawn (default: ignored). Returns
/// `{ url, child, exited, dataDir, stop() }`; `stop()` hard-kills the
/// process and, unless `keepData`, deletes the data dir.
async function spawnServer({ port, user = "test", password = "test-pass", env = {}, stdio = "ignore", dataDir } = {}) {
  dataDir = dataDir || fs.mkdtempSync(path.join(os.tmpdir(), "nyxdb-test-"));
  const url = `http://127.0.0.1:${port}`;
  const child = spawn(BIN, [], {
    env: { ...process.env, NYXDB_USER: user, NYXDB_PASSWORD: password, NYXDB_DATA: dataDir, NYXDB_ADDR: `127.0.0.1:${port}`, ...env },
    stdio,
  });
  const exited = new Promise((resolve) => child.once("exit", resolve));
  const stop = async ({ keepData = false } = {}) => {
    if (child.exitCode === null) child.kill("SIGKILL");
    await exited;
    if (!keepData) removeDir(dataDir);
  };
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`${url}/`)).ok) return { url, child, exited, dataDir, stop, auth: basicAuth(user, password) };
    } catch {}
    if (child.exitCode !== null) break;
    await sleep(50);
  }
  await stop();
  throw new Error(`server did not start on port ${port}`);
}

module.exports = {
  SERVER_URL,
  OUR_USER: USER,
  OUR_PASSWORD: PASSWORD,
  TEST_USER: USER,
  TEST_PASSWORD: PASSWORD,
  AUTH,
  AUTH_HEADER: AUTH,
  AUTH_OPT: { auth: { username: USER, password: PASSWORD } },
  BIN,
  basicAuth,
  check,
  failures,
  finish,
  sleep,
  removeDir,
  residentMB,
  spawnServer,
};
