// The PouchDB the integration tests drive: 8.x by default, 9.x (what
// Offlog ships) with POUCHDB=9. Both are installed here (`pouchdb9` is an
// npm alias for pouchdb@9), so the suite runs against either.
//
// Local (LevelDB) databases go in a per-process temp dir, removed on
// exit: on Windows `destroy()` leaves the folder behind, which used to
// litter the repo with `inttest_*/`-style directories.

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const major = process.env.POUCHDB || "8";
if (major !== "8" && major !== "9") throw new Error(`POUCHDB must be 8 or 9, got ${major}`);
const PouchDB = require(major === "9" ? "pouchdb9" : "pouchdb");
if (!PouchDB.version.startsWith(major + ".")) throw new Error(`asked for PouchDB ${major}.x, loaded ${PouchDB.version}`);
console.log(`(PouchDB ${PouchDB.version})`);

const localDir = fs.mkdtempSync(path.join(os.tmpdir(), "nyxdb-pouch-"));
process.on("exit", () => {
  try {
    fs.rmSync(localDir, { recursive: true, force: true, maxRetries: 5 });
  } catch {}
});

// PouchDB applies `prefix` to local adapters only; http(s) URLs are untouched.
module.exports = PouchDB.defaults({ prefix: localDir + path.sep });
