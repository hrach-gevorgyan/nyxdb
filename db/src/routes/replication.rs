//! The replication endpoints: `_bulk_docs`, `_revs_diff`, `_bulk_get`.

use super::*;

/// `POST /{db}/_bulk_docs` — dispatches to whichever write semantics the
/// client asked for:
/// - default (`new_edits` absent/true): single-writer last-write-wins,
///   the server mints the new rev. Used by app-level writes.
/// - `new_edits:false`: the pushing side of real replication sends its
///   own exact revision tree (`_rev` + `_revisions`); the server must
///   store that verbatim, including creating a real conflict if it
///   diverges from what we already have.
pub(super) async fn bulk_docs(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let db = state.db(&db_name)?;
    let feed = state.feeds.get_or_create(&db_name);
    let docs = payload
        .get("docs")
        .and_then(Value::as_array)
        .ok_or(ApiError(StatusCode::BAD_REQUEST, "bad_request", "bad_request"))?;
    let new_edits = payload.get("new_edits").and_then(Value::as_bool).unwrap_or(true);

    let response = blocking(|| {
        let _gate = db.attachment_writer();
        if new_edits {
            bulk_docs_new_edits_true(&db, &feed, docs)
        } else {
            bulk_docs_new_edits_false(&db, &feed, docs)
        }
    })?;
    // One flush per batch, before acknowledging any of it.
    db.flush().await.map_err(|_| internal_error())?;
    Ok(response)
}

pub(super) fn bulk_docs_new_edits_true(db: &Db, feed: &ChangeFeed, docs: &[Value]) -> Result<Json<Value>, ApiError> {
    // Validate and store attachments first, then write every doc that
    // passed in one batch (one transaction — see `Db::try_update_trees`).
    let mut results: Vec<Option<Value>> = Vec::with_capacity(docs.len());
    let mut writes = Vec::new();
    let mut payloads = Vec::new();
    for doc in docs {
        let mut doc = doc.clone();
        let id = doc
            .get("_id")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if let Some(obj) = doc.as_object_mut() {
            obj.remove("_id");
        }
        let previous = current_body(db, &id);
        match crate::attachments::extract_inline_attachments(db, &mut doc, previous.as_ref()) {
            Ok(()) => {}
            Err(crate::attachments::AttachmentError::Invalid) => {
                results.push(Some(json!({"id": id, "error": "bad_request", "reason": "invalid attachment data"})));
                continue;
            }
            Err(crate::attachments::AttachmentError::MissingStub(reason)) => {
                results.push(Some(json!({"id": id, "error": "missing_stub", "reason": reason})));
                continue;
            }
        }
        let (requested_parent, deleted) = take_write_meta(&mut doc);
        writes.push((results.len(), id));
        payloads.push(Some((requested_parent, deleted, doc)));
        results.push(None);
    }

    let ids: Vec<&str> = writes.iter().map(|(_, id)| id.as_str()).collect();
    let written = db.try_update_trees(&ids, feed, |i, tree| {
        let (requested_parent, deleted, body) = payloads[i].take().expect("each write is applied once");
        Ok::<_, std::convert::Infallible>(apply_app_write(tree, requested_parent, deleted, body))
    });
    for (n, (slot, id)) in writes.iter().enumerate() {
        results[*slot] = Some(match &written {
            Ok(written) => match &written[n] {
                Ok((_, rev)) => json!({"ok": true, "id": id, "rev": rev}),
                Err(never) => match *never {},
            },
            Err(_) => json!({"id": id, "error": "internal_error", "reason": "failed to write document"}),
        });
    }
    Ok(Json(Value::Array(results.into_iter().map(|r| r.expect("every doc has a result")).collect())))
}

/// Real CouchDB returns `[]` on a fully successful `new_edits:false`
/// push (the rev is dictated by the client, so there's nothing new to
/// report) — only failures show up in the response array.
pub(super) fn bulk_docs_new_edits_false(db: &Db, feed: &ChangeFeed, docs: &[Value]) -> Result<Json<Value>, ApiError> {
    let error_row = |id: &str, reason: &'static str| {
        let error = match reason {
            "failed to write document" => "internal_error",
            "missing_stub" => "missing_stub",
            _ => "bad_request",
        };
        json!({"id": id, "error": error, "reason": reason})
    };
    let mut errors = Vec::new();
    let mut ids = Vec::new();
    let mut payloads = Vec::new();
    for doc in docs {
        let Some(id) = doc.get("_id").and_then(Value::as_str).map(String::from) else {
            errors.push(json!({"error": "bad_request", "reason": "missing _id"}));
            continue;
        };
        match prepare_new_edits_false(db, doc) {
            Ok(prepared) => {
                ids.push(id);
                payloads.push(Some(prepared));
            }
            Err(reason) => errors.push(error_row(&id, reason)),
        }
    }
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let written = db.try_update_trees(&id_refs, feed, |i, tree| {
        let (chain, deleted, body) = payloads[i].take().expect("each write is applied once");
        tree.insert_revision_chain(&chain, deleted, body);
        Ok::<_, std::convert::Infallible>(())
    });
    if written.is_err() {
        errors.extend(ids.iter().map(|id| error_row(id, "failed to write document")));
    }
    Ok(Json(Value::Array(errors)))
}

/// A `new_edits:false` doc, ready to insert: its revision chain (newest
/// first), deleted flag, and body with metadata removed and inline
/// attachments stored.
type PreparedRevision = (Vec<crate::revtree::RevId>, bool, Value);

fn prepare_new_edits_false(db: &Db, doc: &Value) -> Result<PreparedRevision, &'static str> {
    let Some(chain) = revision_chain(doc) else {
        return Err("missing _rev");
    };
    let deleted = doc.get("_deleted").and_then(Value::as_bool).unwrap_or(false);
    let mut body = doc.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.remove("_id");
        obj.remove("_rev");
        obj.remove("_revisions");
        obj.remove("_deleted");
    }
    crate::attachments::extract_inline_attachments(db, &mut body, None).map_err(|e| match e {
        crate::attachments::AttachmentError::Invalid => "invalid attachment data",
        crate::attachments::AttachmentError::MissingStub(_) => "missing_stub",
    })?;
    Ok((chain, deleted, body))
}

/// Single-doc `PUT ?new_edits=false`: inserts the client-dictated
/// `_rev`/`_revisions` chain into the document's revision tree verbatim,
/// exactly as real replication does — this is what makes a diverging
/// push create a genuine conflict instead of silently overwriting. Never
/// mints a new revision itself.
pub(super) fn write_doc_new_edits_false(
    db: &Db,
    feed: &ChangeFeed,
    id: &str,
    doc: &Value,
) -> Result<String, &'static str> {
    let (chain, deleted, body) = prepare_new_edits_false(db, doc)?;
    let rev = chain[0].clone();
    db.update_tree(id, feed, |tree| tree.insert_revision_chain(&chain, deleted, body))
        .map(|_| rev)
        .map_err(|_| "failed to write document")
}

/// Resolves a doc's revision id + ancestry (as `new_edits:false` sends
/// it) into the newest-first chain `RevTree::insert_revision_chain`
/// expects. Falls back to a single-node chain (no known ancestry) if
/// `_revisions` wasn't sent alongside `_rev`.
pub(super) fn revision_chain(doc: &Value) -> Option<Vec<crate::revtree::RevId>> {
    let rev = doc.get("_rev").and_then(Value::as_str)?;
    let revisions = doc.get("_revisions").and_then(Value::as_object);
    match revisions {
        Some(revisions) => {
            let start = revisions.get("start")?.as_u64()?;
            let ids = revisions.get("ids")?.as_array()?;
            Some(
                ids.iter()
                    .enumerate()
                    .filter_map(|(i, hash)| {
                        let hash = hash.as_str()?;
                        let gen = start.checked_sub(i as u64)?;
                        Some(format!("{gen}-{hash}"))
                    })
                    .collect(),
            )
        }
        None => Some(vec![rev.to_string()]),
    }
}

/// `POST /{db}/_revs_diff` — given `{docId: [revs...]}`, report which of
/// those revisions we do NOT already have, so the client knows what it
/// actually needs to push. A presence check against the doc's tree; no
/// `possible_ancestors`.
pub(super) async fn revs_diff(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    blocking(|| revs_diff_blocking(&state, &db_name, &payload))
}

pub(super) fn revs_diff_blocking(state: &AppState, db_name: &str, payload: &Value) -> Result<Json<Value>, ApiError> {
    let db = state.db(db_name)?;
    let requested = payload.as_object().ok_or(ApiError(StatusCode::BAD_REQUEST, "bad_request", "bad_request"))?;

    let mut result = serde_json::Map::new();
    for (doc_id, revs) in requested {
        let revs: Vec<String> = revs
            .as_array()
            .map(|arr| arr.iter().filter_map(|r| r.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let tree = db.get_tree(doc_id).map_err(|_| internal_error())?.unwrap_or_default();
        let missing: Vec<&String> = tree.missing(&revs);
        if !missing.is_empty() {
            result.insert(doc_id.clone(), json!({"missing": missing}));
        }
    }
    Ok(Json(Value::Object(result)))
}

/// `POST /{db}/_bulk_get` — given `[{"id":..,"rev":..(optional)}, ...]`,
/// fetch each doc body in one round-trip instead of N individual `GET`s
///. If `rev` is omitted, returns the current winner; deleted
/// docs/missing revs report as `error` entries rather than failing the
/// whole batch, matching CouchDB's per-item error shape.
#[derive(serde::Deserialize, Default)]
pub(super) struct BulkGetParams {
    revs: Option<bool>,
    latest: Option<bool>,
}

/// `revs`/`latest` can arrive in the query string (CouchDB's documented
/// form, what PouchDB sends) or as top-level fields of the request body
/// (also seen from clients) — either one turns the option on.
pub(super) async fn bulk_get(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Query(params): Query<BulkGetParams>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    blocking(|| bulk_get_blocking(&state, &db_name, &params, &payload))
}

pub(super) fn bulk_get_blocking(state: &AppState, db_name: &str, params: &BulkGetParams, payload: &Value) -> Result<Json<Value>, ApiError> {
    let db = state.db(db_name)?;
    let requests = payload
        .get("docs")
        .and_then(Value::as_array)
        .ok_or(ApiError(StatusCode::BAD_REQUEST, "bad_request", "bad_request"))?;
    let flag = |query: Option<bool>, key: &str| query.unwrap_or(false) || payload.get(key).and_then(Value::as_bool).unwrap_or(false);
    let revs = flag(params.revs, "revs");
    let latest = flag(params.latest, "latest");

    let mut results = Vec::with_capacity(requests.len());
    for req in requests {
        let Some(id) = req.get("id").and_then(Value::as_str) else {
            results.push(json!({"id": Value::Null, "docs": [{"error": {"error": "bad_request", "reason": "missing id"}}]}));
            continue;
        };
        let requested_rev = req.get("rev").and_then(Value::as_str);

        let docs = (|| -> StorageResult<Vec<Value>> {
            let Some(tree) = db.get_tree(id)? else {
                return Ok(vec![json!({"error": {"id": id, "error": "not_found", "reason": "missing"}})]);
            };
            let rev_ids = match requested_rev {
                Some(rev) if latest => match tree.leaves_descending_from(rev) {
                    leaves if leaves.is_empty() => vec![rev.to_string()],
                    leaves => leaves,
                },
                Some(rev) => vec![rev.to_string()],
                None => match tree.winner() {
                    Some(rev) => vec![rev.clone()],
                    None => return Ok(vec![json!({"error": {"id": id, "error": "not_found", "reason": "missing"}})]),
                },
            };
            Ok(rev_ids.iter().map(|rev_id| match tree.nodes.get(rev_id).filter(|_| tree.is_leaf(rev_id)) {
                // A specific (or resolved-winner) revision that exists in
                // the tree is a real, fetchable revision even if it's a
                // tombstone — real CouchDB returns it as an "ok" result
                // with `_deleted:true` in the body, never an error here.
                // Confirmed directly against CouchDB 3.5.2: this holds
                // both for an explicitly-requested deleted leaf (a losing
                // conflict branch) and for a resolved winner that's
                // itself a tombstone (a fully-deleted document) — `_bulk_get`
                // never uses the "reason":"deleted" error PouchDB's own
                // `getDocs()` treats as a hard batch failure (`doc.error`
                // aborts the whole replication batch), unlike plain
                // `GET /{db}/{id}` (see `get_doc`), which does 404 on a
                // deleted winner since that's asking "does this document
                // currently exist," a different question. Fixed after
                // this exact mismatch broke real replication — see
                // `doc/changelog.md`.
                Some(node) => json!({"ok": outgoing_body(&db, &tree, id, rev_id, node, revs)}),
                None => json!({"error": {"id": id, "rev": rev_id, "error": "not_found", "reason": "missing"}}),
            }).collect())
        })()
        .map_err(|_| internal_error())?;

        results.push(json!({"id": id, "docs": docs}));
    }
    Ok(Json(json!({"results": results})))
}
