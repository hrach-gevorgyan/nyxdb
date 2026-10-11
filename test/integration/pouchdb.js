// The PouchDB the integration tests drive: 8.x by default, 9.x (what
// Offlog ships) with POUCHDB=9. Both are installed here (`pouchdb9` is an
// npm alias for pouchdb@9), so the suite runs against either.

const major = process.env.POUCHDB || "8";
if (major !== "8" && major !== "9") throw new Error(`POUCHDB must be 8 or 9, got ${major}`);
const PouchDB = require(major === "9" ? "pouchdb9" : "pouchdb");
if (!PouchDB.version.startsWith(major + ".")) throw new Error(`asked for PouchDB ${major}.x, loaded ${PouchDB.version}`);
console.log(`(PouchDB ${PouchDB.version})`);
module.exports = PouchDB;
