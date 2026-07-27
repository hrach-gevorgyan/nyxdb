//! Attachment handling (Phase 4). Real CouchDB stores attachment bytes
//! alongside the document; PouchDB's http adapter can send them either
//! inline as base64 JSON (`_attachments.<name>.data`) or via
//! `multipart/related`. This server only supports the inline JSON form
//! — no multipart parsing — which covers what a PouchDB client sends by
//! default and keeps the implementation to plain JSON throughout, this
//! project's whole premise.
//!
//! On write, inline attachment data is decoded, hashed, stored
//! separately in `Db::attachments` (content-addressed by digest), and
//! replaced in the document body with a small stub. On read, stubs are
//! returned as-is unless the caller asks for full data
//! (`?attachments=true`), in which case they're re-inflated back to
//! inline base64.

use crate::storage::Db;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn digest_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256-{:x}", hasher.finalize())
}

/// Used by the standalone `PUT /{db}/{id}/{attname}` endpoint, which
/// receives raw bytes directly (not inline JSON base64) — hashes and
/// stores them the same way `extract_inline_attachments` does, so both
/// paths produce identical stubs for identical content.
pub fn store_raw_attachment(db: &Db, bytes: &[u8]) -> Result<String, String> {
    let digest = digest_of(bytes);
    db.put_attachment(&digest, bytes).map_err(|e| e.to_string())?;
    Ok(digest)
}

/// Scans `body["_attachments"]` for inline base64 data (`"data"` field,
/// no `"stub"`), decodes and stores each one, then replaces it with a
/// stub. Already-stubbed entries (`"stub":true`, no `"data"` — e.g. from
/// a client that fetched a doc without `?attachments=true` and is now
/// writing it back unchanged) are left as-is, since the bytes are
/// already stored under that digest.
///
/// Returns an error message for the first malformed attachment found
/// (bad base64), rather than partially writing the rest — a document's
/// write should be all-or-nothing.
pub fn extract_inline_attachments(db: &Db, body: &mut Value) -> Result<(), String> {
    let Some(attachments) = body.get_mut("_attachments").and_then(Value::as_object_mut) else {
        return Ok(());
    };

    for (name, attachment) in attachments.iter_mut() {
        let Some(obj) = attachment.as_object_mut() else { continue };
        let Some(data_b64) = obj.get("data").and_then(Value::as_str).map(String::from) else {
            continue; // already a stub, or malformed — leave alone either way
        };

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&data_b64)
            .map_err(|e| format!("attachment {name:?}: invalid base64: {e}"))?;
        let digest = digest_of(&bytes);
        let length = bytes.len();
        db.put_attachment(&digest, &bytes).map_err(|e| format!("attachment {name:?}: {e}"))?;

        let content_type = obj.get("content_type").cloned().unwrap_or(json!("application/octet-stream"));
        *attachment = json!({
            "content_type": content_type,
            "digest": digest,
            "length": length,
            "stub": true,
        });
    }
    Ok(())
}

/// For `GET ...?attachments=true`: replaces every stub in
/// `body["_attachments"]` with inline base64 data, fetched from
/// storage by digest. A stub whose digest is missing from storage
/// (shouldn't normally happen) is left as a stub rather than failing
/// the whole request.
pub fn inflate_attachments(db: &Db, body: &mut Value) {
    let Some(attachments) = body.get_mut("_attachments").and_then(Value::as_object_mut) else {
        return;
    };

    for attachment in attachments.values_mut() {
        let Some(obj) = attachment.as_object_mut() else { continue };
        let Some(digest) = obj.get("digest").and_then(Value::as_str).map(String::from) else { continue };
        let Ok(Some(bytes)) = db.get_attachment(&digest) else { continue };
        obj.insert("data".to_string(), json!(base64::engine::general_purpose::STANDARD.encode(&bytes)));
        obj.remove("stub");
    }
}
