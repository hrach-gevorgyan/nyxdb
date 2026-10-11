// Databases are only created by `PUT /{db}`, and only with a valid name
// (see doc/changelog.md, v0.1.7).
//
// Every other route used to silently create the database it named — a
// typo'd db name in a client's sync URL produced an empty database and a
// "successful" sync into it instead of an error. CouchDB answers those
// with 404 {"error":"not_found","reason":"Database does not exist."}.
//
// No dependencies (native fetch).
// Requires: SERVER_URL/OUR_USER/OUR_PASSWORD (defaults: 127.0.0.1:8085, testuser/testpass)

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const OUR_USER = process.env.OUR_USER || "testuser";
const OUR_PASSWORD = process.env.OUR_PASSWORD || "testpass";
const AUTH = "Basic " + Buffer.from(`${OUR_USER}:${OUR_PASSWORD}`).toString("base64");

let failures = 0;
function check(name, condition, detail) {
  if (condition) console.log(`PASS: ${name}`);
  else {
    console.error(`FAIL: ${name}${detail ? " — " + detail : ""}`);
    failures++;
  }
}

async function req(method, path, body) {
  const resp = await fetch(`${SERVER_URL}${path}`, {
    method,
    headers: { "Content-Type": "application/json", Authorization: AUTH },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await resp.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    json = text;
  }
  return { status: resp.status, body: json };
}

const DOES_NOT_EXIST = "Database does not exist.";

async function main() {
  const missing = "missingdb_" + Date.now();
  const probes = [
    ["GET", `/${missing}`],
    ["GET", `/${missing}/doc`],
    ["PUT", `/${missing}/doc`, { a: 1 }],
    ["PUT", `/${missing}/doc?new_edits=false`, { _id: "doc", _rev: "1-a" }],
    ["POST", `/${missing}/_bulk_docs`, { docs: [{ _id: "x" }] }],
    ["POST", `/${missing}/_bulk_get`, { docs: [{ id: "x" }] }],
    ["POST", `/${missing}/_revs_diff`, { x: ["1-a"] }],
    ["GET", `/${missing}/_changes`],
    ["GET", `/${missing}/_local/ckpt`],
    ["PUT", `/${missing}/_local/ckpt`, { last_seq: 1 }],
    ["GET", `/${missing}/doc/att.txt`],
  ];
  for (const [method, path, body] of probes) {
    const r = await req(method, path, body);
    check(
      `${method} ${path.replace(missing, "<missing>")} on a missing db is 404 "Database does not exist."`,
      r.status === 404 && r.body?.error === "not_found" && r.body?.reason === DOES_NOT_EXIST,
      `${r.status} ${JSON.stringify(r.body)}`
    );
  }
  const after = await req("GET", `/${missing}`);
  check("none of those requests created the database", after.status === 404, `${after.status} ${JSON.stringify(after.body)}`);

  for (const bad of ["_users", "_replicator", "Uppercase", "9starts_with_digit", "has%20space", "semi;colon"]) {
    const r = await req("PUT", `/${bad}`);
    check(
      `PUT /${decodeURIComponent(bad)} is rejected 400 illegal_database_name`,
      r.status === 400 && r.body?.error === "illegal_database_name",
      `${r.status} ${JSON.stringify(r.body)}`
    );
  }

  const good = `good_db-1$(x)+y_${Date.now()}`;
  const created = await req("PUT", `/${encodeURIComponent(good)}`);
  const info = await req("GET", `/${encodeURIComponent(good)}`);
  check(
    "a name using every allowed character class is accepted",
    (created.status === 200 || created.status === 201) && info.status === 200,
    `${created.status} ${JSON.stringify(created.body)} / ${info.status}`
  );
  await req("DELETE", `/${encodeURIComponent(good)}`);

  if (failures > 0) {
    console.error(`\n${failures} check(s) failed`);
    process.exit(1);
  }
  console.log("\nDatabases are only created by PUT /{db}, with valid names");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
