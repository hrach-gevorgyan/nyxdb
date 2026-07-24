//! HTTP routes. Phase 0 scope only (plan §8): server identification,
//! `PUT/GET /{db}`, `GET/PUT /{db}/{id}`, `POST /{db}/_bulk_docs`. No
//! revision-tree conflict handling wired up yet — last-write-wins.

use crate::revtree::RevNode;
use crate::storage::Db;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub root: Arc<sled::Db>,
}

/// CouchDB clients (PouchDB included) expect a JSON body on error
/// responses, e.g. `{"error":"not_found","reason":"missing"}` — a bare
/// status code with an empty body fails their JSON parsing.
struct ApiError(StatusCode, &'static str);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let reason = match self.0 {
            StatusCode::NOT_FOUND => "missing",
            StatusCode::PRECONDITION_FAILED => "file_exists",
            StatusCode::BAD_REQUEST => "bad_request",
            _ => "internal_error",
        };
        (self.0, Json(json!({"error": self.1, "reason": reason}))).into_response()
    }
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not_found")
}

fn internal_error() -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(server_info))
        .route("/:db", put(create_db).get(db_info))
        .route("/:db/:id", get(get_doc).put(put_doc))
        .route("/:db/_bulk_docs", post(bulk_docs))
        .route("/:db/_revs_diff", post(revs_diff))
        .route("/:db/_local/:id", get(get_local).put(put_local))
        .with_state(state)
}

async fn server_info() -> Json<Value> {
    Json(json!({
        "couchdb": "Welcome",
        "version": env!("CARGO_PKG_VERSION"),
        "uuid": uuid::Uuid::new_v4().to_string(),
    }))
}

async fn create_db(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let docs_tree_name = format!("{db_name}::docs");
    let existed = state.root.tree_names().iter().any(|n| n == docs_tree_name.as_bytes());
    if existed {
        return Err(ApiError(StatusCode::PRECONDITION_FAILED, "file_exists"));
    }
    Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    Ok(Json(json!({"ok": true})))
}

async fn db_info(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    let doc_count = db.docs.len();
    Ok(Json(json!({"db_name": db_name, "doc_count": doc_count})))
}

async fn get_doc(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let db = Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    let tree = db.get_tree(&id).map_err(|_| internal_error())?;
    let tree = tree.ok_or_else(not_found)?;
    let winner = tree.winner().ok_or_else(not_found)?;
    let node = &tree.nodes[winner];
    let mut body = node.body.clone();
    body["_id"] = json!(id);
    body["_rev"] = json!(winner);
    Ok(Json(body))
}

async fn put_doc(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let db = Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    let rev = write_doc(&db, &id, body).map_err(|_| internal_error())?;
    Ok(Json(json!({"ok": true, "id": id, "rev": rev})))
}

/// `GET/PUT /{db}/_local/{id}` — replication checkpoints (plan §3, §4.5).
/// Single-revision, last-write-wins, never appear in `_changes`.
async fn get_local(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let db = Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    let bytes = db.local.get(&id).map_err(|_| internal_error())?;
    let bytes = bytes.ok_or_else(not_found)?;
    let mut body: Value = serde_json::from_slice(&bytes).map_err(|_| internal_error())?;
    body["_id"] = json!(format!("_local/{id}"));
    Ok(Json(body))
}

async fn put_local(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
    Json(mut body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let db = Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    if let Some(obj) = body.as_object_mut() {
        obj.remove("_id");
    }
    let bytes = serde_json::to_vec(&body).map_err(|_| internal_error())?;
    db.local.insert(&id, bytes).map_err(|_| internal_error())?;
    Ok(Json(json!({"ok": true, "id": format!("_local/{id}"), "rev": "0-1"})))
}

/// `POST /{db}/_bulk_docs` — dispatches to whichever write semantics the
/// client asked for (plan §3, §8):
/// - default (`new_edits` absent/true): single-writer last-write-wins,
///   the server mints the new rev. Used by app-level writes.
/// - `new_edits:false`: the pushing side of real replication sends its
///   own exact revision tree (`_rev` + `_revisions`); the server must
///   store that verbatim, including creating a real conflict if it
///   diverges from what we already have (plan §2.3, §4).
async fn bulk_docs(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let db = Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    let docs = payload
        .get("docs")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "bad_request"))?;
    let new_edits = payload.get("new_edits").and_then(Value::as_bool).unwrap_or(true);

    if new_edits {
        bulk_docs_new_edits_true(&db, docs)
    } else {
        bulk_docs_new_edits_false(&db, docs)
    }
}

fn bulk_docs_new_edits_true(db: &Db, docs: &[Value]) -> Result<Json<Value>, ApiError> {
    let mut results = Vec::with_capacity(docs.len());
    for doc in docs {
        let mut doc = doc.clone();
        let id = doc
            .get("_id")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if let Some(obj) = doc.as_object_mut() {
            obj.remove("_id");
            obj.remove("_rev");
        }

        match write_doc(db, &id, doc) {
            Ok(rev) => results.push(json!({"ok": true, "id": id, "rev": rev})),
            Err(_) => results.push(json!({
                "id": id,
                "error": "internal_error",
                "reason": "failed to write document",
            })),
        }
    }
    Ok(Json(Value::Array(results)))
}

/// Real CouchDB returns `[]` on a fully successful `new_edits:false`
/// push (the rev is dictated by the client, so there's nothing new to
/// report) — only failures show up in the response array.
fn bulk_docs_new_edits_false(db: &Db, docs: &[Value]) -> Result<Json<Value>, ApiError> {
    let mut errors = Vec::new();
    for doc in docs {
        let Some(id) = doc.get("_id").and_then(Value::as_str) else {
            errors.push(json!({"error": "bad_request", "reason": "missing _id"}));
            continue;
        };
        let Some(chain) = revision_chain(doc) else {
            errors.push(json!({"id": id, "error": "bad_request", "reason": "missing _rev"}));
            continue;
        };
        let deleted = doc.get("_deleted").and_then(Value::as_bool).unwrap_or(false);
        let mut body = doc.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.remove("_id");
            obj.remove("_rev");
            obj.remove("_revisions");
            obj.remove("_deleted");
        }

        let write = || -> sled::Result<()> {
            let mut tree = db.get_tree(id)?.unwrap_or_default();
            tree.insert_revision_chain(&chain, deleted, body);
            db.put_tree(id, &tree)?;
            Ok(())
        };
        if write().is_err() {
            errors.push(json!({
                "id": id,
                "error": "internal_error",
                "reason": "failed to write document",
            }));
        }
    }
    Ok(Json(Value::Array(errors)))
}

/// Resolves a doc's revision id + ancestry (as `new_edits:false` sends
/// it) into the newest-first chain `RevTree::insert_revision_chain`
/// expects. Falls back to a single-node chain (no known ancestry) if
/// `_revisions` wasn't sent alongside `_rev`.
fn revision_chain(doc: &Value) -> Option<Vec<crate::revtree::RevId>> {
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
/// actually needs to push (plan §3). Phase 0: simple presence check
/// against the doc's current tree, no `possible_ancestors` computation.
async fn revs_diff(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let db = Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    let requested = payload.as_object().ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "bad_request"))?;

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

fn write_doc(db: &Db, id: &str, body: Value) -> sled::Result<String> {
    let mut tree = db.get_tree(id)?.unwrap_or_default();
    let parent = tree.winner().cloned();
    let gen = parent
        .as_ref()
        .and_then(|r| r.split_once('-'))
        .and_then(|(g, _)| g.parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    let hash = format!("{:x}", md5_like(&body));
    let rev = format!("{gen}-{hash}");
    tree.nodes.insert(rev.clone(), RevNode { parent, deleted: false, body });
    db.put_tree(id, &tree)?;
    Ok(rev)
}

fn md5_like(body: &Value) -> u64 {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(body.to_string().as_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[0..8].try_into().unwrap())
}
