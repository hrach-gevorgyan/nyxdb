//! Single-document reads and writes, design docs, and the shared
//! write path (`write_doc`, rev hashing).

use super::*;

/// `/{db}/_design/{name}`: design docs are ordinary docs whose id holds a
/// `/`, which the `/:db/:id` route can't capture.
pub(super) fn design_id(db_name: String, name: String) -> Path<(String, String)> {
    Path((db_name, format!("_design/{name}")))
}

pub(super) async fn get_design(
    state: State<AppState>,
    Path((db_name, name)): Path<(String, String)>,
    params: Query<GetDocParams>,
) -> Result<Json<Value>, ApiError> {
    get_doc(state, design_id(db_name, name), params).await
}

pub(super) async fn put_design(
    state: State<AppState>,
    Path((db_name, name)): Path<(String, String)>,
    params: Query<PutDocParams>,
    body: Json<Value>,
) -> Result<Json<Value>, ApiError> {
    put_doc(state, design_id(db_name, name), params, body).await
}

pub(super) async fn delete_design(
    state: State<AppState>,
    Path((db_name, name)): Path<(String, String)>,
    params: Query<DeleteDocParams>,
) -> Result<Json<Value>, ApiError> {
    delete_doc(state, design_id(db_name, name), params).await
}

/// The body of one stored revision, as every read path returns it:
/// `_id`/`_rev`, `_deleted:true` for a tombstone, and — when asked for
/// with `revs=true` — `_revisions` (`{start, ids}`, hash parts newest
/// first). Without `_revisions`, a pulling PouchDB stores an incoming
/// revision as a new root beside the copy it already has, turning every
/// edit of an already-replicated doc into a phantom conflict — see
/// `doc/changelog.md` (v0.1.6).
pub(super) fn revision_body(tree: &RevTree, id: &str, rev: &str, node: &RevNode, revs: bool) -> Value {
    let mut body = node.body.clone();
    body["_id"] = json!(id);
    body["_rev"] = json!(rev);
    if node.deleted {
        body["_deleted"] = json!(true);
    }
    if revs {
        if let Some((start, ids)) = tree.revisions(rev) {
            body["_revisions"] = json!({"start": start, "ids": ids});
        }
    }
    body
}

/// `revision_body` plus the attachment-digest presentation every
/// outgoing doc needs (see `attachments::present_digests`).
pub(super) fn outgoing_body(db: &Db, tree: &RevTree, id: &str, rev: &str, node: &RevNode, revs: bool) -> Value {
    let mut body = revision_body(tree, id, rev, node, revs);
    crate::attachments::present_digests(db, &mut body);
    body
}

/// The current winner's body, if it's live — what a bare `{"stub":true}`
/// on an app write refers back to.
pub(super) fn current_body(db: &Db, id: &str) -> Option<Value> {
    let tree = db.get_tree(id).ok().flatten()?;
    let winner = tree.winner()?;
    let node = &tree.nodes[winner];
    (!node.deleted).then(|| node.body.clone())
}

pub(super) fn attachment_error(err: crate::attachments::AttachmentError) -> ApiError {
    match err {
        crate::attachments::AttachmentError::Invalid => ApiError(StatusCode::BAD_REQUEST, "bad_request", "invalid attachment data"),
        crate::attachments::AttachmentError::MissingStub(_) => {
            ApiError(StatusCode::PRECONDITION_FAILED, "missing_stub", "attachment stub refers to data the server does not have")
        }
    }
}

#[derive(serde::Deserialize)]
pub(super) struct GetDocParams {
    #[serde(default)]
    conflicts: bool,
    #[serde(default)]
    attachments: bool,
    #[serde(default)]
    revs: bool,
    #[serde(default)]
    revs_info: bool,
    #[serde(default)]
    latest: bool,
    /// `all`, or a JSON array of rev ids.
    open_revs: Option<String>,
    /// A specific revision instead of the winner.
    rev: Option<String>,
}

/// Picks the revision a single-doc read is about: `rev` if given (404
/// `missing` if it isn't stored — a pulling PouchDB asks for each branch
/// by rev, and must never silently get another branch's content), else
/// the winner (404 `deleted` if the winner is a tombstone).
pub(super) fn resolve_rev<'t>(tree: &'t RevTree, rev: Option<&str>) -> Result<(&'t RevId, &'t RevNode), ApiError> {
    match rev {
        // Only leaves have content (see `RevTree::prune`): a non-leaf rev is
        // as unreadable as an unknown one, like CouchDB after compaction.
        Some(rev) if tree.is_leaf(rev) => tree.nodes.get_key_value(rev).ok_or_else(not_found),
        Some(_) => Err(not_found()),
        None => {
            let winner = tree.winner().ok_or_else(not_found)?;
            let node = &tree.nodes[winner];
            if node.deleted {
                return Err(deleted());
            }
            Ok((winner, node))
        }
    }
}

pub(super) async fn get_doc(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
    Query(params): Query<GetDocParams>,
) -> Result<Json<Value>, ApiError> {
    blocking(|| get_doc_blocking(&state, &db_name, &id, &params))
}

pub(super) fn get_doc_blocking(state: &AppState, db_name: &str, id: &str, params: &GetDocParams) -> Result<Json<Value>, ApiError> {
    let db = state.db(db_name)?;
    let tree = db.get_tree(id).map_err(|_| internal_error())?;
    let tree = tree.ok_or_else(not_found)?;
    if let Some(open_revs) = params.open_revs.as_deref() {
        return open_revs_response(&db, &tree, id, open_revs, params).map(Json);
    }
    let (winner, node) = resolve_rev(&tree, params.rev.as_deref())?;
    let mut body = outgoing_body(&db, &tree, id, winner, node, params.revs);
    if params.revs_info {
        let info: Vec<Value> = tree
            .ancestry(winner)
            .into_iter()
            .map(|rev| {
                let status = match tree.nodes.get(&rev) {
                    Some(_) if !tree.is_leaf(&rev) => "missing",
                    Some(n) if n.deleted => "deleted",
                    Some(_) => "available",
                    None => "missing",
                };
                json!({"rev": rev, "status": status})
            })
            .collect();
        body["_revs_info"] = json!(info);
    }
    if params.conflicts {
        let conflicts = tree.conflicts();
        if !conflicts.is_empty() {
            body["_conflicts"] = json!(conflicts);
        }
    }
    if params.attachments {
        crate::attachments::inflate_attachments(&db, &mut body);
    }
    Ok(Json(body))
}

/// `GET /{db}/{id}?open_revs=...` — CouchDB's documented JSON shape (what
/// PouchDB asks for with `Accept: application/json`): an array with one
/// `{"ok": doc}` per revision found and `{"missing": rev}` per requested
/// rev that isn't stored. `all` means every leaf, tombstones included.
/// With `latest=true`, each requested rev is replaced by the leaves
/// descending from it. Honours `revs=true` and `attachments=true`.
pub(super) fn open_revs_response(
    db: &Db,
    tree: &RevTree,
    id: &str,
    open_revs: &str,
    params: &GetDocParams,
) -> Result<Value, ApiError> {
    let bad_request = || ApiError(StatusCode::BAD_REQUEST, "bad_request", "invalid open_revs");
    let (requested, all): (Vec<String>, bool) = if open_revs == "all" {
        let mut leaves: Vec<String> = tree.leaves().into_iter().cloned().collect();
        leaves.sort();
        (leaves, true)
    } else {
        (serde_json::from_str(open_revs).map_err(|_| bad_request())?, false)
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for rev in requested {
        let found = if params.latest && !all { tree.leaves_descending_from(&rev) } else { vec![rev.clone()] };
        let found: Vec<String> = found.into_iter().filter(|r| tree.is_leaf(r)).collect();
        if found.is_empty() {
            out.push(json!({"missing": rev}));
            continue;
        }
        for r in found {
            if !seen.insert(r.clone()) {
                continue;
            }
            let mut body = outgoing_body(db, tree, id, &r, &tree.nodes[&r], params.revs);
            if params.attachments {
                crate::attachments::inflate_attachments(db, &mut body);
            }
            out.push(json!({"ok": body}));
        }
    }
    Ok(Value::Array(out))
}

#[derive(serde::Deserialize)]
pub(super) struct PutDocParams {
    new_edits: Option<bool>,
}

/// `PUT /{db}/{id}` — two distinct write semantics, same as `_bulk_docs`
///, selected by `?new_edits=`:
/// - default (absent/true): single-writer last-write-wins, the server
///   mints the new rev on top of the current winner. Used by app-level
///   writes.
/// - `new_edits=false`: the client dictates the exact `_rev`/`_revisions`
///   ancestry — must be inserted into the tree verbatim, creating a real
///   conflict if it diverges from what's already there, exactly like
///   `_bulk_docs` with `new_edits:false`. Before this fix, `PUT` ignored
///   `?new_edits=` entirely and always took the last-write-wins path,
///   silently discarding the client's `_rev` and never forking a
///   conflict — see `doc/changelog.md`.
pub(super) async fn put_doc(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
    Query(params): Query<PutDocParams>,
    Json(mut body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let db = state.db(&db_name)?;
    let feed = state.feeds.get_or_create(&db_name);

    if params.new_edits == Some(false) {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("_id".to_string(), json!(id));
        }
        let rev = blocking(|| {
            let _gate = db.attachment_writer();
            write_doc_new_edits_false(&db, &feed, &id, &body)
        })
        .map_err(|reason| match reason {
            "missing _rev" => ApiError(StatusCode::BAD_REQUEST, "bad_request", "missing _rev"),
            "invalid attachment data" => ApiError(StatusCode::BAD_REQUEST, "bad_request", "invalid attachment data"),
            "missing_stub" => attachment_error(crate::attachments::AttachmentError::MissingStub(String::new())),
            _ => internal_error(),
        })?;
        db.flush().await.map_err(|_| internal_error())?;
        return Ok(Json(json!({"ok": true, "id": id, "rev": rev})));
    }

    let rev = blocking(|| {
        let _gate = db.attachment_writer();
        let previous = current_body(&db, &id);
        crate::attachments::extract_inline_attachments(&db, &mut body, previous.as_ref()).map_err(attachment_error)?;
        write_doc(&db, &feed, &id, body).map_err(|_| internal_error())
    })?;
    db.flush().await.map_err(|_| internal_error())?;
    Ok(Json(json!({"ok": true, "id": id, "rev": rev})))
}

/// The app-write path (`new_edits` absent/true): mints the next rev on
/// top of whatever the winner is *at write time* — read inside the
/// write lock, so two concurrent writers build a line instead of both
/// branching off the same parent and one overwriting the other.
/// `_id`, `_rev` and `_deleted` are request metadata, not content: they're
/// taken out of the body before it's stored (a stale `_rev` used to be
/// stored as a field of the new revision, and `_deleted:true` was stored
/// as an ordinary field on a *live* revision instead of deleting).
pub(super) fn take_write_meta(body: &mut Value) -> (Option<String>, bool) {
    let Some(obj) = body.as_object_mut() else { return (None, false) };
    obj.remove("_id");
    let rev = obj.remove("_rev").and_then(|r| r.as_str().map(String::from));
    let deleted = obj.remove("_deleted").and_then(|d| d.as_bool()).unwrap_or(false);
    (rev, deleted)
}

/// The app-write path (`new_edits` absent/true): mints the next rev. Its
/// parent is the body's `_rev` when that's a current leaf — so an edit or
/// delete of a losing branch lands on that branch, which is how clients
/// resolve conflicts — and otherwise the winner *at write time* (read
/// inside the write lock, so concurrent writers build a line). There's
/// still no optimistic concurrency: a stale `_rev` doesn't fail the write
/// (see doc/USAGE.md §7). `_deleted:true` writes a tombstone.
pub(super) fn write_doc(db: &Db, feed: &ChangeFeed, id: &str, mut body: Value) -> StorageResult<String> {
    let (requested_parent, deleted) = take_write_meta(&mut body);
    db.update_tree(id, feed, |tree| apply_app_write(tree, requested_parent, deleted, body)).map(|(_, rev)| rev)
}

/// The in-lock half of an app write: picks the parent (the requested
/// `_rev` if it's a current leaf, else the winner), mints the new rev and
/// adds it. Shared by `write_doc` and `_bulk_docs`.
pub(super) fn apply_app_write(tree: &mut RevTree, requested_parent: Option<String>, deleted: bool, body: Value) -> String {
    let parent = match requested_parent {
        Some(rev) if tree.is_leaf(&rev) => Some(rev),
        _ => tree.winner().cloned(),
    };
    let gen = parent
        .as_ref()
        .and_then(|r| r.split_once('-'))
        .and_then(|(g, _)| g.parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    let rev = format!("{gen}-{:x}", rev_hash(parent.as_deref(), deleted, &body));
    tree.nodes.insert(rev.clone(), RevNode { parent, deleted, body });
    rev
}

#[derive(serde::Deserialize)]
pub(super) struct DeleteDocParams {
    rev: Option<String>,
}

pub(super) fn conflict() -> ApiError {
    ApiError(StatusCode::CONFLICT, "conflict", "Document update conflict.")
}

/// `DELETE /{db}/{id}?rev=<rev>` — writes a tombstone on top of `rev`,
/// which must be a current leaf (409 otherwise). On a conflicted doc,
/// deleting a losing leaf removes that branch: this is the request
/// PouchDB's `db.remove(id, losingRev)` sends to resolve a conflict on a
/// remote, and it used to get 405 (the route didn't exist). Without
/// `rev`, the live winner is deleted. The leaf check happens inside the
/// write lock, so a concurrent write can't redirect the delete.
pub(super) async fn delete_doc(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
    Query(params): Query<DeleteDocParams>,
) -> Result<Json<Value>, ApiError> {
    let db = state.db(&db_name)?;
    let feed = state.feeds.get_or_create(&db_name);
    let written = blocking(|| {
        db.try_update_tree(&id, &feed, |tree| {
            if tree.nodes.is_empty() {
                return Err(not_found());
            }
            let parent = match params.rev.as_deref() {
                Some(rev) if tree.is_leaf(rev) => rev.to_string(),
                Some(_) => return Err(conflict()),
                None => match tree.winner() {
                    Some(winner) if !tree.nodes[winner].deleted => winner.clone(),
                    _ => return Err(deleted()),
                },
            };
            let gen = parent.split_once('-').and_then(|(g, _)| g.parse::<u64>().ok()).unwrap_or(0) + 1;
            let body = json!({});
            let rev = format!("{gen}-{:x}", rev_hash(Some(&parent), true, &body));
            tree.nodes.insert(rev.clone(), RevNode { parent: Some(parent), deleted: true, body });
            Ok(rev)
        })
    })
    .map_err(|_| internal_error())?;
    let (_, rev) = written?;
    db.flush().await.map_err(|_| internal_error())?;
    Ok(Json(json!({"ok": true, "id": id, "rev": rev})))
}

/// Like CouchDB's, the rev hash covers the parent and the deleted flag as
/// well as the body — otherwise two tombstones with the same (empty) body
/// at the same generation on different branches would get the same rev
/// id and collide in the tree.
pub(super) fn rev_hash(parent: Option<&str>, deleted: bool, body: &Value) -> u64 {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(parent.unwrap_or("").as_bytes());
    hasher.update([deleted as u8]);
    hasher.update(body.to_string().as_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[0..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Request metadata must not be stored as document content: a stale
    /// `_rev` used to be saved as a field of the new revision (invisible
    /// over HTTP, since reads overwrite `_rev`, but stored and hashed).
    #[test]
    fn write_metadata_is_taken_out_of_the_stored_body() {
        let mut body = json!({"_id": "x", "_rev": "1-old", "_deleted": true, "keep": 1});
        let (rev, deleted) = take_write_meta(&mut body);
        assert_eq!(rev.as_deref(), Some("1-old"));
        assert!(deleted);
        assert_eq!(body, json!({"keep": 1}));
    }
}
