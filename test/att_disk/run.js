// Attachment bytes must not pile up on disk (see doc/changelog.md,
// v0.1.8). In v0.1.7 they lived in sled: ~2.6x disk use that was never
// reclaimed, unreferenced blobs that POST /{db}/_compact couldn't free,
// and DELETE /{db} left the bytes readable in sled's files.
//
// This test owns its server (port 8291, NYXDB_ATTACHMENT_GRACE_SECS=0 so
// cleanup doesn't wait an hour) and measures its data directory:
// - replacing every attachment, then _compact: old bytes freed;
// - deleting every doc, then _compact: all bytes freed;
// - DELETE /{db}: the attachment content is gone from every file on disk.
//
// Requires a built binary: NYXDB_BIN (default target/release/nyxdb[.exe]).

const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const { check, finish, removeDir, spawnServer } = require("../lib/harness.js");

const PORT = Number(process.env.ATT_DISK_PORT || 8291);
const DOCS = 20;
const SIZE = 1024 * 1024;
const MB = 1024 * 1024;

function dirBytes(dir) {
  if (!fs.existsSync(dir)) return 0;
  let total = 0;
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    total += e.isDirectory() ? dirBytes(p) : fs.statSync(p).size;
  }
  return total;
}

/// Whether any file under `dir` contains `needle`.
function containsBytes(dir, needle) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory() ? containsBytes(p, needle) : fs.readFileSync(p).includes(needle)) return true;
  }
  return false;
}

async function main() {
  const server = await spawnServer({ port: PORT, user: "disk", password: "disk", env: { NYXDB_ATTACHMENT_GRACE_SECS: "0" } });
  const call = async (method, p, body, type = "application/json") => {
    const r = await fetch(`${server.url}/disk${p}`, {
      method,
      headers: { Authorization: server.auth, "Content-Type": type },
      body: body === undefined ? undefined : Buffer.isBuffer(body) ? body : JSON.stringify(body),
    });
    const text = await r.text();
    return { status: r.status, body: text ? JSON.parse(text) : null };
  };
  const mb = (bytes) => `${(bytes / MB).toFixed(1)} MB`;
  try {
    await call("PUT", "");
    const revs = [];
    const marker = crypto.randomBytes(64); // every first-round attachment carries it
    const upload = async (i, rev) => {
      const bytes = crypto.randomBytes(SIZE);
      if (rev === undefined) marker.copy(bytes, 4096);
      const r = await call("PUT", `/doc${i}/file.bin${rev ? `?rev=${rev}` : ""}`, bytes, "application/octet-stream");
      if (r.status >= 300) throw new Error(`upload ${i}: ${r.status} ${JSON.stringify(r.body)}`);
      return r.body.rev;
    };
    for (let i = 0; i < DOCS; i++) revs[i] = await upload(i);
    const afterUpload = dirBytes(server.dataDir);

    for (let i = 0; i < DOCS; i++) revs[i] = await upload(i, revs[i]);
    await call("POST", "/_compact", {});
    const afterReplace = dirBytes(server.dataDir);
    check(
      `replacing ${DOCS} × 1 MB attachments then _compact frees the old bytes`,
      afterReplace < afterUpload + 0.3 * DOCS * SIZE,
      `${mb(afterUpload)} after upload, ${mb(afterReplace)} after replacing + _compact`
    );

    for (let i = 0; i < DOCS; i++) await call("DELETE", `/doc${i}?rev=${revs[i]}`);
    await call("POST", "/_compact", {});
    const afterDelete = dirBytes(server.dataDir);
    check(
      "deleting every doc then _compact frees their attachment bytes",
      afterDelete < afterUpload - 0.7 * DOCS * SIZE,
      `${mb(afterUpload)} after upload, ${mb(afterDelete)} after deleting + _compact`
    );

    // Uploads racing back-to-back sweeps (grace 0): every attachment that
    // was acknowledged must still be there, conflict branches included.
    let sweeping = true;
    const sweeps = (async () => {
      let n = 0;
      while (sweeping) {
        await call("POST", "/_compact", {});
        n++;
      }
      return n;
    })();
    const uploaded = [];
    for (let i = 0; i < 60; i++) {
      const bytes = crypto.randomBytes(64 * 1024);
      const r = await call("PUT", `/race${i}/file.bin`, bytes, "application/octet-stream");
      if (r.status >= 300) throw new Error(`race upload ${i}: ${r.status}`);
      uploaded.push([`/race${i}/file.bin`, bytes]);
    }
    const branchBytes = crypto.randomBytes(64 * 1024);
    const h = (c) => c.repeat(32);
    await call("POST", "/_bulk_docs", {
      new_edits: false,
      docs: [
        { _id: "branchy", _rev: `1-${h("a")}`, _revisions: { start: 1, ids: [h("a")] } },
        { _id: "branchy", _rev: `2-${h("b")}`, _revisions: { start: 2, ids: [h("b"), h("a")] }, _attachments: { "f.bin": { content_type: "application/octet-stream", data: branchBytes.toString("base64") } } },
        { _id: "branchy", _rev: `2-${h("c")}`, _revisions: { start: 2, ids: [h("c"), h("a")] } },
      ],
    });
    uploaded.push([`/branchy/f.bin?rev=2-${h("b")}`, branchBytes]);
    sweeping = false;
    const sweepCount = await sweeps;
    await call("POST", "/_compact", {});
    const missing = [];
    for (const [p, bytes] of uploaded) {
      const r = await fetch(`${server.url}/disk${p}`, { headers: { Authorization: server.auth } });
      const got = Buffer.from(await r.arrayBuffer());
      if (r.status !== 200 || !got.equals(bytes)) missing.push(`${p} (${r.status})`);
    }
    check(
      `${uploaded.length} attachments uploaded during ${sweepCount} back-to-back sweeps all survive, conflict branch included`,
      missing.length === 0 && sweepCount > 1,
      `${missing.length} missing: ${missing.slice(0, 3).join(", ")}; ${sweepCount} sweeps`
    );

    // Fresh content for the DELETE /{db} check (the first round may be gone already).
    for (let i = 0; i < 3; i++) await upload(100 + i);
    await call("DELETE", "");
    // sled keeps its files locked while running (Windows): scan after stopping.
    await server.stop({ keepData: true });
    check(
      "after DELETE /{db}, none of its attachment content is left in any file on disk",
      !containsBytes(server.dataDir, marker),
      "the 64-byte marker from the uploads is still on disk"
    );
  } finally {
    await server.stop();
    removeDir(server.dataDir);
  }
  finish("Attachment disk space is reclaimed");
}

main().catch((err) => {
  console.error("ERROR:", err.stack || err);
  process.exit(1);
});
