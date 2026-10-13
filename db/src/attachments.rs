//! Attachment handling. Real CouchDB stores attachment bytes
//! alongside the document; PouchDB's http adapter can send them either
//! inline as base64 JSON (`_attachments.<name>.data`) or via
//! `multipart/related`. This server only supports the inline JSON form
//! — no multipart parsing — which covers what a PouchDB client sends by
//! default and keeps the implementation to plain JSON throughout, this
//! project's whole premise.
//!
//! On write, inline attachment data is decoded, stored separately in
//! `Db::attachments`, and replaced in the document body with a small
//! stub. On read, stubs are returned as-is unless the caller asks for
//! full data (`?attachments=true`), in which case they're re-inflated
//! back to inline base64.
//!
//! Digests: stubs carry CouchDB's `md5-<base64>` form, because that's
//! what PouchDB computes for its own copies and what its replicator
//! compares to decide whether it already has an attachment — with any
//! other format every pull re-downloaded every attachment. Bytes stay
//! content-addressed by sha256 internally (an md5 → sha256 index maps
//! between them); stubs written before v0.1.7 still carry a `sha256-`
//! digest and are shown as md5 on the way out (`present_digests`).

use crate::storage::Db;
use base64::Engine;
use md5::Md5;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Internal content address: what `Db::attachments` is keyed by.
fn content_key(bytes: &[u8]) -> String {
    format!("sha256-{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
pub(crate) fn content_key_for_tests(bytes: &[u8]) -> String {
    content_key(bytes)
}

/// CouchDB/PouchDB-style digest, as exposed in stubs.
pub fn md5_digest(bytes: &[u8]) -> String {
    format!("md5-{}", base64::engine::general_purpose::STANDARD.encode(Md5::digest(bytes)))
}

pub enum AttachmentError {
    /// Malformed attachment data (e.g. bad base64) — 400.
    Invalid,
    /// A stub naming bytes the server doesn't have — CouchDB's 412
    /// `missing_stub`.
    MissingStub(String),
}

/// Stores raw bytes and returns their client-facing md5 digest. Used by
/// the standalone `PUT /{db}/{id}/{attname}` endpoint and by inline
/// extraction, so both produce identical stubs for identical content.
pub fn store_raw_attachment(db: &Db, bytes: &[u8]) -> Result<String, String> {
    let md5 = md5_digest(bytes);
    db.put_attachment(&content_key(bytes), &md5, bytes).map_err(|e| e.to_string())?;
    Ok(md5)
}

/// Scans `body["_attachments"]`. Inline data (`"data"`) is decoded,
/// stored, and replaced with a stub. A stub (no `"data"`) is a claim that
/// the server already has those bytes, so it's checked:
///
/// - with a digest: the digest must be stored, else `MissingStub` —
///   storing it anyway would leave an attachment nobody can ever read;
/// - without a digest (a plain `PUT` re-sending `{"stub":true}` for an
///   attachment it isn't changing): the same-named attachment of
///   `previous` (the current winner's body) is kept, else `MissingStub`.
///
/// All-or-nothing: the first bad entry fails the whole document.
pub fn extract_inline_attachments(db: &Db, body: &mut Value, previous: Option<&Value>) -> Result<(), AttachmentError> {
    let Some(attachments) = body.get_mut("_attachments").and_then(Value::as_object_mut) else {
        return Ok(());
    };

    for (name, attachment) in attachments.iter_mut() {
        let Some(obj) = attachment.as_object_mut() else { continue };
        let Some(data_b64) = obj.get("data").and_then(Value::as_str).map(String::from) else {
            match obj.get("digest").and_then(Value::as_str) {
                Some(digest) => {
                    if !db.has_attachment(digest).unwrap_or(false) {
                        return Err(AttachmentError::MissingStub(format!("{name}: unknown digest {digest}")));
                    }
                }
                None => {
                    let kept = previous
                        .and_then(|p| p.get("_attachments"))
                        .and_then(|a| a.get(name))
                        .filter(|a| a.get("digest").is_some())
                        .cloned()
                        .ok_or_else(|| AttachmentError::MissingStub(format!("{name}: no previous attachment")))?;
                    *attachment = kept;
                }
            }
            continue;
        };

        let bytes = base64::engine::general_purpose::STANDARD.decode(&data_b64).map_err(|_| AttachmentError::Invalid)?;
        let digest = store_raw_attachment(db, &bytes).map_err(|_| AttachmentError::Invalid)?;
        let content_type = obj.get("content_type").cloned().unwrap_or(json!("application/octet-stream"));
        *attachment = json!({
            "content_type": content_type,
            "digest": digest,
            "length": bytes.len(),
            "stub": true,
        });
    }
    Ok(())
}

/// Rewrites any legacy `sha256-` stub digest in an outgoing body to the
/// md5 form clients compare against (computed from the stored bytes, and
/// indexed so later stubs carrying it resolve). New stubs are already md5.
pub fn present_digests(db: &Db, body: &mut Value) {
    let Some(attachments) = body.get_mut("_attachments").and_then(Value::as_object_mut) else {
        return;
    };
    for attachment in attachments.values_mut() {
        let Some(obj) = attachment.as_object_mut() else { continue };
        let Some(digest) = obj.get("digest").and_then(Value::as_str).map(String::from) else { continue };
        if !digest.starts_with("sha256-") {
            continue;
        }
        let Ok(Some(bytes)) = db.get_attachment(&digest) else { continue };
        let md5 = md5_digest(&bytes);
        let _ = db.put_attachment(&digest, &md5, &bytes);
        obj.insert("digest".to_string(), json!(md5));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::WriteLocks;

    /// Stubs written before v0.1.7 carry the sha256 content key as their
    /// digest. They must still resolve, and go out as md5.
    #[test]
    fn legacy_sha256_stubs_still_resolve_and_are_shown_as_md5() {
        let att = crate::storage::TestDir::new();
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, &att.0, "legacy", &WriteLocks::default()).unwrap();
        let bytes = b"written by v0.1.6";
        let legacy = content_key(bytes);
        db.legacy_attachments.insert(legacy.as_bytes(), &bytes[..]).unwrap();

        assert!(db.has_attachment(&legacy).unwrap());
        let mut body = json!({"_attachments": {"f": {"stub": true, "digest": legacy, "length": bytes.len()}}});
        present_digests(&db, &mut body);
        let shown = body["_attachments"]["f"]["digest"].as_str().unwrap().to_string();
        assert_eq!(shown, md5_digest(bytes));
        assert_eq!(db.get_attachment(&shown).unwrap().as_deref(), Some(&bytes[..]), "the md5 it was shown as must resolve too");
    }
}
