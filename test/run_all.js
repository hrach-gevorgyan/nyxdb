// Runs the whole regression: release build (zero warnings), clippy, unit
// tests, then every suite against one scratch server, then the
// PouchDB-driven suites again on PouchDB 9.x. Exit code 0 only if all of
// it passed.
//
//   node test/run_all.js            everything
//   node test/run_all.js --no-rust  skip build/clippy/unit tests (binary must exist)
//   node test/run_all.js feeds ops  only suites whose path contains one of these
//
// Needs `npm install --prefix test/integration` once. The scratch server
// gets its own port (RUN_ALL_PORT, default 8281) and a temp data dir that
// is deleted afterwards; suites that spawn their own servers use their
// own fixed ports (see test/README.md).

const { spawnSync, spawn } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnServer } = require("./lib/harness.js");

const REPO = path.join(__dirname, "..");
const PORT = Number(process.env.RUN_ALL_PORT || 8281);

// Order: fast, self-contained checks first; long soak tests last.
const SUITES = [
  "test/startup/run.js",
  "test/memory/run.js",
  "test/att_memory/run.js",
  "test/att_disk/run.js",
  "test/migration/run.js",
  "test/blocking/run.js",
  "test/durability/run.js",
  "test/ops/run.js",
  "test/databases/run.js",
  "test/write_semantics/run.js",
  "test/read_surface/run.js",
  "test/changes_follow/run.js",
  "test/feeds/run.js",
  "test/revisions/run.js",
  "test/concurrency/run.js",
  "test/perf/run.js",
  "test/ported/pouchdb_tests.js",
  "test/attachments/run.js",
  "test/load/run.js",
  "test/integration/run.js",
  "test/integration/live_sync.js",
  "test/integration/edit_replication.js",
  "test/integration/delete_replication.js",
  "test/integration/checkpoint_resume.js",
  "test/integration/large_batches.js",
];
// Suites that drive a real PouchDB client: run again on 9.x.
const POUCHDB_SUITES = SUITES.filter((s) => s.startsWith("test/integration/") || s === "test/attachments/run.js" || s === "test/write_semantics/run.js");

const args = process.argv.slice(2);
const rust = !args.includes("--no-rust");
const filters = args.filter((a) => !a.startsWith("--"));
const selected = (list) => (filters.length ? list.filter((s) => filters.some((f) => s.includes(f))) : list);

const results = [];
function record(name, ok, detail = "") {
  results.push({ name, ok });
  console.log(`${ok ? "ok  " : "FAIL"}  ${name}${detail ? "  " + detail : ""}`);
}

function cargo(argsList) {
  return spawnSync("cargo", [...argsList, "--manifest-path", path.join(REPO, "db", "Cargo.toml")], { cwd: REPO, encoding: "utf8" });
}

function rustChecks() {
  for (const [name, cmd] of [
    ["build (release, zero warnings)", ["build", "--release"]],
    ["clippy (zero findings)", ["clippy", "--release", "--all-targets"]],
  ]) {
    const r = cargo(cmd);
    const lines = r.stderr.split("\n");
    const isFinding = (l) => l.startsWith("warning") || l.startsWith("error");
    const findings = lines.filter(isFinding).length;
    record(name, r.status === 0 && findings === 0, findings ? `${findings} finding(s)` : "");
    // Each finding, plus its "--> file:line" location line.
    if (r.status !== 0 || findings) console.log(lines.filter((l) => isFinding(l) || l.trimStart().startsWith("-->")).slice(0, 20).join("\n"));
    if (r.status !== 0) return false;
  }
  const t = cargo(["test", "--release"]);
  const summary = (t.stdout.match(/test result: .*/) || [""])[0];
  record("unit tests", t.status === 0, summary.replace(/; finished.*/, ""));
  return true;
}

function runSuite(file, env) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [path.join(REPO, file)], { cwd: REPO, env: { ...process.env, ...env } });
    let out = "";
    child.stdout.on("data", (d) => (out += d));
    child.stderr.on("data", (d) => (out += d));
    child.on("exit", (code) => resolve({ code, out }));
  });
}

/// The v0.1.7 binary, for the upgrade test (test/migration): built once
/// from the v0.1.7 tag into target/compat and reused after that.
function oldBinary() {
  const exe = process.platform === "win32" ? "nyxdb.exe" : "nyxdb";
  const out = path.join(REPO, "target", "compat", "v0.1.7", exe);
  if (fs.existsSync(out)) return out;
  const src = path.join(REPO, "target", "compat", "src-v0.1.7");
  const git = (...args) => spawnSync("git", args, { cwd: REPO, encoding: "utf8" });
  if (!fs.existsSync(src)) {
    const added = git("worktree", "add", "--detach", src, "v0.1.7");
    if (added.status !== 0) throw new Error(`can't check out v0.1.7 (fetch the tags?): ${added.stderr}`);
  }
  const built = spawnSync("cargo", ["build", "--release", "--manifest-path", path.join(src, "db", "Cargo.toml"), "--target-dir", path.join(REPO, "target", "compat", "build")], {
    cwd: REPO,
    encoding: "utf8",
  });
  if (built.status !== 0) throw new Error(`building v0.1.7 failed: ${built.stderr.slice(-2000)}`);
  fs.mkdirSync(path.dirname(out), { recursive: true });
  fs.copyFileSync(path.join(REPO, "target", "compat", "build", "release", exe), out);
  git("worktree", "remove", "--force", src);
  return out;
}

async function suites(server) {
  const env = {
    NYXDB_OLD_BIN: selected(SUITES).includes("test/migration/run.js") ? oldBinary() : "",
    SERVER_URL: server.url,
    OUR_USER: "runall",
    OUR_PASSWORD: "runall-pass",
    TEST_USER: "runall",
    TEST_PASSWORD: "runall-pass",
  };
  const passes = [
    ["", selected(SUITES), {}],
    ["[pouchdb 9] ", selected(POUCHDB_SUITES), { POUCHDB: "9" }],
  ];
  for (const [label, list, extra] of passes) {
    for (const file of list) {
      const started = Date.now();
      const { code, out } = await runSuite(file, { ...env, ...extra });
      record(label + file, code === 0, `${((Date.now() - started) / 1000).toFixed(1)} s`);
      if (code !== 0) {
        // Every FAIL/ERROR line, wherever it is, plus the last few lines.
        const lines = out.trim().split("\n");
        const shown = [...new Set([...lines.filter((l) => /^(FAIL|ERROR)/.test(l)), ...lines.slice(-4)])];
        console.log(shown.map((l) => "      " + l).join("\n"));
      }
    }
  }
}

async function main() {
  if (filters.length && !selected(SUITES).length) throw new Error(`no suite matches ${filters.join(", ")}`);
  if (rust && !rustChecks()) return;
  const logFile = path.join(os.tmpdir(), `nyxdb-run-all-${process.pid}.log`);
  const log = fs.openSync(logFile, "w");
  // Grace 0: suites check that _compact frees replaced attachments right
  // away; the one-hour default is covered by unit tests.
  const server = await spawnServer({
    port: PORT,
    user: "runall",
    password: "runall-pass",
    stdio: ["ignore", log, log],
    env: { NYXDB_ATTACHMENT_GRACE_SECS: "0" },
  });
  try {
    await suites(server);
  } finally {
    await server.stop();
    fs.closeSync(log);
  }
  if (results.some((r) => !r.ok)) console.log(`\nserver log kept: ${logFile}`);
  else fs.rmSync(logFile, { force: true });
}

main()
  .catch((err) => record("runner", false, err.stack || String(err)))
  .finally(() => {
    const failed = results.filter((r) => !r.ok).length;
    console.log(`\n${results.length - failed}/${results.length} passed${failed ? `, ${failed} FAILED` : ""}`);
    process.exitCode = failed ? 1 : 0;
  });
