//! HTTP routes. Phase 0 scope only (plan §8): server identification,
//! `PUT/GET /{db}`, `GET/PUT /{db}/{id}`, `POST /{db}/_bulk_docs`. No
//! revision-tree conflict handling wired up yet — last-write-wins.

use crate::revtree::RevNode;
use crate::storage::Db;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post, put},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub root: Arc<sled::Db>,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(server_info))
        .route("/:db", put(create_db).get(db_info))
        .route("/:db/:id", get(get_doc).put(put_doc))
        .route("/:db/_bulk_docs", post(bulk_docs))
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
) -> Result<Json<Value>, StatusCode> {
    let docs_tree_name = format!("{db_name}::docs");
    let existed = state.root.tree_names().iter().any(|n| n == docs_tree_name.as_bytes());
    if existed {
        return Err(StatusCode::PRECONDITION_FAILED);
    }
    Db::open(&state.root, &db_name).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({"ok": true})))
}

async fn db_info(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let db = Db::open(&state.root, &db_name).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let doc_count = db.docs.len();
    Ok(Json(json!({"db_name": db_name, "doc_count": doc_count})))
}

async fn get_doc(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
) -> Result<Json<Value>, StatusCode> {
    let db = Db::open(&state.root, &db_name).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let tree = db.get_tree(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let tree = tree.ok_or(StatusCode::NOT_FOUND)?;
    let winner = tree.winner().ok_or(StatusCode::NOT_FOUND)?;
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
) -> Result<Json<Value>, StatusCode> {
    let db = Db::open(&state.root, &db_name).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let rev = write_doc(&db, &id, body).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({"ok": true, "id": id, "rev": rev})))
}

/// `POST /{db}/_bulk_docs` — Phase 0 scope: batch writes using the same
/// last-write-wins generation logic as `put_doc`. Does not yet honor
/// `new_edits:false` (accepting the client's own revision tree verbatim),
/// which real replication needs — that's Phase 1 (plan §4, §8).
async fn bulk_docs(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let db = Db::open(&state.root, &db_name).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let docs = payload
        .get("docs")
        .and_then(Value::as_array)
        .ok_or(StatusCode::BAD_REQUEST)?;

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

        match write_doc(&db, &id, doc) {
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
