//! `_local` documents (replication checkpoints).

use super::*;

/// `GET/PUT /{db}/_local/{id}` — replication checkpoints.
/// Single-revision, last-write-wins, never appear in `_changes`.
pub(super) async fn get_local(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    blocking(|| {
        let db = state.db(&db_name)?;
        let bytes = db.local.get(&id).map_err(|_| internal_error())?;
        let bytes = bytes.ok_or_else(not_found)?;
        let mut body: Value = serde_json::from_slice(&bytes).map_err(|_| internal_error())?;
        body["_id"] = json!(format!("_local/{id}"));
        if body.get("_rev").is_none() {
            body["_rev"] = json!("0-1"); // stored before revisions were counted
        }
        Ok(Json(body))
    })
}

/// The `N` of a stored `_local` doc's `0-N` revision (0 if there's none).
pub(super) fn local_rev_number(stored: Option<&[u8]>) -> u64 {
    stored
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
        .and_then(|doc| doc.get("_rev")?.as_str()?.strip_prefix("0-")?.parse().ok())
        .unwrap_or(0)
}

/// `_local` docs (replication checkpoints) are single-revision, never
/// replicated and never in `_changes`. Like CouchDB, each write gets the
/// next `0-N` revision — it used to be `0-1` forever — and the client's
/// own `_id`/`_rev` aren't stored as fields. The increment is one atomic
/// sled update, so concurrent checkpoint writes can't hand out the same
/// number. No optimistic concurrency, same as documents.
pub(super) async fn put_local(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
    Json(mut body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let db = state.db(&db_name)?;
    if let Some(obj) = body.as_object_mut() {
        obj.remove("_id");
        obj.remove("_rev");
    }
    let stored = blocking(|| {
        db.local.update_and_fetch(&id, |old| {
            let mut next = body.clone();
            next["_rev"] = json!(format!("0-{}", local_rev_number(old) + 1));
            serde_json::to_vec(&next).ok()
        })
    })
    .map_err(|_| internal_error())?;
    db.flush().await.map_err(|_| internal_error())?;
    let rev = format!("0-{}", local_rev_number(stored.as_deref()));
    Ok(Json(json!({"ok": true, "id": format!("_local/{id}"), "rev": rev})))
}

pub(super) async fn delete_local(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let db = state.db(&db_name)?;
    let removed = blocking(|| db.local.remove(&id)).map_err(|_| internal_error())?;
    if removed.is_none() {
        return Err(not_found());
    }
    db.flush().await.map_err(|_| internal_error())?;
    Ok(Json(json!({"ok": true, "id": format!("_local/{id}"), "rev": "0-0"})))
}
