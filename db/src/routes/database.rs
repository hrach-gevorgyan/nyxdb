//! Database-level endpoints: create/info/delete, `_all_docs`, `_compact`.

use super::*;

pub(super) async fn create_db(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    blocking(|| create_db_blocking(&state, &db_name))
}

pub(super) fn create_db_blocking(state: &AppState, db_name: &str) -> Result<Json<Value>, ApiError> {
    let db_name = db_name.to_string();
    if !crate::storage::valid_db_name(&db_name) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "illegal_database_name",
            "Only lowercase characters (a-z), digits (0-9), and any of the characters _, $, (, ), +, -, and / are allowed. Must begin with a letter.",
        ));
    }
    if crate::storage::db_exists(&state.root, &db_name) {
        return Err(ApiError(StatusCode::PRECONDITION_FAILED, "file_exists", "The database could not be created, the file already exists."));
    }
    Db::open(&state.root, &db_name, &state.locks).map_err(|_| internal_error())?;
    Ok(Json(json!({"ok": true})))
}

pub(super) async fn db_info(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    blocking(|| {
        let db = state.db(&db_name)?;
        // CouchDB's `doc_count` is live docs only; tombstones are counted
        // separately. This scans every tree — fine at this server's scale.
        let (mut live, mut deleted) = (0u64, 0u64);
        for id in db.doc_ids().map_err(|_| internal_error())? {
            match winner_state(&db, &id)? {
                Some((_, false)) => live += 1,
                Some((_, true)) => deleted += 1,
                None => {}
            }
        }
        let update_seq = db.current_seq().map_err(|_| internal_error())?;
        Ok(Json(json!({
            "db_name": db_name,
            "doc_count": live,
            "doc_del_count": deleted,
            "update_seq": update_seq,
            "instance_start_time": "0",
        })))
    })
}

/// A doc's winning rev and whether it is a tombstone.
pub(super) fn winner_state(db: &Db, id: &str) -> Result<Option<(String, bool)>, ApiError> {
    let Some(tree) = db.get_tree(id).map_err(|_| internal_error())? else { return Ok(None) };
    Ok(tree.winner().map(|rev| (rev.to_string(), tree.nodes[rev].deleted)))
}

#[derive(serde::Deserialize)]
pub(super) struct AllDocsParams {
    #[serde(default)]
    include_docs: bool,
    #[serde(default)]
    descending: bool,
    limit: Option<usize>,
    #[serde(default)]
    skip: usize,
    /// JSON-encoded, like CouchDB (`startkey="a"`); `start_key`/`end_key` too.
    #[serde(alias = "start_key")]
    startkey: Option<String>,
    #[serde(alias = "end_key")]
    endkey: Option<String>,
}

#[derive(serde::Deserialize)]
pub(super) struct AllDocsKeys {
    keys: Vec<String>,
}

pub(super) fn json_key(raw: Option<&str>) -> Result<Option<String>, ApiError> {
    raw.map(|raw| {
        serde_json::from_str::<String>(raw)
            .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "bad_request", "startkey/endkey must be JSON strings"))
    })
    .transpose()
}

pub(super) fn all_docs_row(db: &Db, id: &str, rev: &str, include_docs: bool) -> Result<Value, ApiError> {
    let mut row = json!({"id": id, "key": id, "value": {"rev": rev}});
    if include_docs {
        let tree = db.get_tree(id).map_err(|_| internal_error())?.ok_or_else(not_found)?;
        let (rev, node) = resolve_rev(&tree, None)?;
        row["doc"] = outgoing_body(db, &tree, id, rev, node, false);
    }
    Ok(row)
}

/// `GET /{db}/_all_docs` — live docs in id order (tombstones left out),
/// with CouchDB's inclusive `startkey`/`endkey`, `descending`, `skip`,
/// `limit` and `include_docs`.
pub(super) async fn all_docs(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Query(params): Query<AllDocsParams>,
) -> Result<Json<Value>, ApiError> {
    blocking(|| {
        let db = state.db(&db_name)?;
        let (start, end) = (json_key(params.startkey.as_deref())?, json_key(params.endkey.as_deref())?);
        let mut live = Vec::new();
        for id in db.doc_ids().map_err(|_| internal_error())? {
            if let Some((rev, false)) = winner_state(&db, &id)? {
                live.push((id, rev));
            }
        }
        let total_rows = live.len();
        if params.descending {
            live.reverse();
        }
        // With `descending`, startkey is the high end and endkey the low.
        let in_range = |id: &str| {
            let (lo, hi) = if params.descending { (&end, &start) } else { (&start, &end) };
            lo.as_deref().is_none_or(|lo| id >= lo) && hi.as_deref().is_none_or(|hi| id <= hi)
        };
        let rows = live
            .iter()
            .filter(|(id, _)| in_range(id))
            .skip(params.skip)
            .take(params.limit.unwrap_or(usize::MAX))
            .map(|(id, rev)| all_docs_row(&db, id, rev, params.include_docs))
            .collect::<Result<Vec<_>, _>>()?;
        let offset = live.iter().take_while(|(id, _)| !in_range(id)).count() + params.skip;
        Ok(Json(json!({"total_rows": total_rows, "offset": offset, "rows": rows})))
    })
}

/// `POST /{db}/_all_docs` with `{"keys": [...]}` — one row per key, in
/// request order: unknown ids get `error: not_found`, deleted ones
/// `value.deleted: true`.
pub(super) async fn all_docs_keys(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Query(params): Query<AllDocsParams>,
    Json(body): Json<AllDocsKeys>,
) -> Result<Json<Value>, ApiError> {
    blocking(|| {
        let db = state.db(&db_name)?;
        let mut rows = Vec::with_capacity(body.keys.len());
        for id in &body.keys {
            rows.push(match winner_state(&db, id)? {
                None => json!({"key": id, "error": "not_found"}),
                Some((rev, true)) => json!({"id": id, "key": id, "value": {"rev": rev, "deleted": true}, "doc": null}),
                Some((rev, false)) => all_docs_row(&db, id, &rev, params.include_docs)?,
            });
        }
        let total_rows = db.doc_ids().map_err(|_| internal_error())?.len();
        Ok(Json(json!({"total_rows": total_rows, "offset": 0, "rows": rows})))
    })
}

/// `POST /{db}/_compact` — deletes attachment bytes no stored revision
/// refers to any more (see `Db::compact_attachments`). CouchDB answers
/// 202 and compacts in the background; this runs to completion first.
pub(super) async fn compact(State(state): State<AppState>, Path(db_name): Path<String>) -> Result<Response, ApiError> {
    blocking(|| {
        let db = state.db(&db_name)?;
        let removed = db.compact_attachments().map_err(|_| internal_error())?;
        tracing::info!("compacted {db_name}: {removed} unreferenced attachment(s) deleted");
        Ok((StatusCode::ACCEPTED, Json(json!({"ok": true}))).into_response())
    })
}

/// `DELETE /{db}` — not part of the replication protocol proper (a
/// PouchDB client never calls this against a remote it's syncing with),
/// but a reasonable, low-risk admin operation to support — mainly useful
/// for test/dev hygiene (tearing down disposable databases) rather than
/// anything client-driven sync depends on.
pub(super) async fn delete_db(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    blocking(|| {
        if !crate::storage::db_exists(&state.root, &db_name) {
            return Err(db_not_found());
        }
        for suffix in ["docs", "local", "seq", "seqidx", "attachments", "attmd5"] {
            state.root.drop_tree(format!("{db_name}::{suffix}")).map_err(|_| internal_error())?;
        }
        Ok(Json(json!({"ok": true})))
    })
}
