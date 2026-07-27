// Benchmark this server against a real CouchDB instance: write/read
// throughput and on-disk footprint for identical workloads. This is a
// one-off comparison script (not a pass/fail test) - it prints numbers,
// doesn't assert against them. Uses disposable databases on both
// servers, deleted at the end - never touches any pre-existing db.
//
// Requires:
//   SERVER_URL/OUR_USER/OUR_PASSWORD     - default http://127.0.0.1:8085, testuser/testpass
//   COUCH_URL/COUCH_USER/COUCH_PASSWORD - default http://127.0.0.1:5984, no default creds (required)
//   BENCH_DOC_COUNT                    - default 5000

const SERVER_URL = process.env.SERVER_URL || "http://127.0.0.1:8085";
const OUR_USER = process.env.OUR_USER || "testuser";
const OUR_PASSWORD = process.env.OUR_PASSWORD || "testpass";
const COUCH_URL = process.env.COUCH_URL || "http://127.0.0.1:5984";
const COUCH_USER = process.env.COUCH_USER;
const COUCH_PASSWORD = process.env.COUCH_PASSWORD;
const DOC_COUNT = Number(process.env.BENCH_DOC_COUNT || 5000);
const DB_NAME = "benchmark_" + Date.now();

if (!COUCH_USER || !COUCH_PASSWORD) {
  console.error("set COUCH_USER/COUCH_PASSWORD to your real CouchDB's admin credentials");
  process.exit(1);
}

function authHeader(user, pass) {
  return "Basic " + Buffer.from(`${user}:${pass}`).toString("base64");
}

// Realistic small document - similar shape to a task-manager record
// (the kind of app this server was designed for), not a toy {"a":1}.
function makeDoc(i) {
  return {
    _id: `doc-${i}`,
    title: `Task number ${i}`,
    done: i % 3 === 0,
    tags: ["work", "personal", "urgent"].slice(0, (i % 3) + 1),
    createdAt: new Date(2026, 0, 1 + (i % 28)).toISOString(),
    notes: "Some free-text notes describing this task in a bit more detail than just a title.",
  };
}

class Server {
  constructor(name, baseUrl, user, password) {
    this.name = name;
    this.baseUrl = baseUrl;
    this.auth = authHeader(user, password);
  }

  async req(method, path, body) {
    const resp = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers: { "Content-Type": "application/json", Authorization: this.auth },
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

  async createDb() {
    const r = await this.req("PUT", `/${DB_NAME}`);
    if (r.status !== 201 && r.status !== 200) {
      throw new Error(`${this.name}: could not create db: ${r.status} ${JSON.stringify(r.body)}`);
    }
  }

  async deleteDb() {
    await this.req("DELETE", `/${DB_NAME}`);
  }

  async bulkInsert(docs) {
    const start = Date.now();
    const r = await this.req("POST", `/${DB_NAME}/_bulk_docs`, { docs });
    const elapsed = Date.now() - start;
    if (r.status !== 201 && r.status !== 200) {
      throw new Error(`${this.name}: bulk insert failed: ${r.status} ${JSON.stringify(r.body)}`);
    }
    return elapsed;
  }

  async sequentialReads(ids) {
    const start = Date.now();
    for (const id of ids) {
      const r = await this.req("GET", `/${DB_NAME}/${id}`);
      if (r.status !== 200) throw new Error(`${this.name}: read ${id} failed: ${r.status}`);
    }
    return Date.now() - start;
  }

  async diskSize() {
    // Real CouchDB reports this directly; not all servers do.
    const r = await this.req("GET", `/${DB_NAME}`);
    return r.body.sizes?.file ?? null;
  }
}

async function main() {
  const ours = new Server("ours", SERVER_URL, OUR_USER, OUR_PASSWORD);
  const couch = new Server("couchdb", COUCH_URL, COUCH_USER, COUCH_PASSWORD);

  console.log(`Benchmarking with ${DOC_COUNT} documents...\n`);

  await ours.createDb();
  await couch.createDb();

  const docs = Array.from({ length: DOC_COUNT }, (_, i) => makeDoc(i));

  const ourWriteMs = await ours.bulkInsert(docs);
  const couchWriteMs = await couch.bulkInsert(docs);
  console.log("=== Bulk write (single _bulk_docs request) ===");
  console.log(`  ours:    ${ourWriteMs}ms  (${(DOC_COUNT / (ourWriteMs / 1000)).toFixed(0)} docs/sec)`);
  console.log(`  couchdb: ${couchWriteMs}ms  (${(DOC_COUNT / (couchWriteMs / 1000)).toFixed(0)} docs/sec)`);
  console.log(`  ratio: ours is ${(couchWriteMs / ourWriteMs).toFixed(2)}x ${ourWriteMs < couchWriteMs ? "faster" : "slower"}\n`);

  const readSampleIds = Array.from({ length: 200 }, (_, i) => `doc-${i * Math.floor(DOC_COUNT / 200)}`);
  const ourReadMs = await ours.sequentialReads(readSampleIds);
  const couchReadMs = await couch.sequentialReads(readSampleIds);
  console.log("=== Sequential single-doc GET (200 requests) ===");
  console.log(`  ours:    ${ourReadMs}ms  (${(ourReadMs / 200).toFixed(2)}ms/req avg)`);
  console.log(`  couchdb: ${couchReadMs}ms  (${(couchReadMs / 200).toFixed(2)}ms/req avg)`);
  console.log(`  ratio: ours is ${(couchReadMs / ourReadMs).toFixed(2)}x ${ourReadMs < couchReadMs ? "faster" : "slower"}\n`);

  const couchDiskBytes = await couch.diskSize();
  console.log("=== On-disk size for the same data (CouchDB-reported, see doc/USAGE.md for how ours was measured separately) ===");
  console.log(`  couchdb: ${couchDiskBytes} bytes (${(couchDiskBytes / 1024 / 1024).toFixed(2)} MB) for ${DOC_COUNT} docs`);

  await ours.deleteDb();
  await couch.deleteDb();
  console.log("\n(both benchmark databases deleted)");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
