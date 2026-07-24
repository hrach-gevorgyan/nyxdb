//! HTTP routes. Phase 0 scope only (plan §8): server identification,
//! `PUT/GET /{db}`, `GET/PUT /{db}/{id}`, `POST /{db}/_bulk_docs`. No
//! revision-tree conflict handling wired up yet — last-write-wins.

use crate::changes::{ChangeEvent, ChangeFeed, ChangeFeedRegistry};
use crate::revtree::RevNode;
use crate::storage::Db;
use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct AppState {
    pub root: Arc<sled::Db>,
    pub feeds: ChangeFeedRegistry,
    pub creds: Arc<crate::auth::Credentials>,
}

/// CouchDB clients (PouchDB included) expect a JSON body on error
/// responses, e.g. `{"error":"not_found","reason":"missing"}` — a bare
/// status code with an empty body fails their JSON parsing.
struct ApiError(StatusCode, &'static str, &'static str);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1, "reason": self.2}))).into_response()
    }
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not_found", "missing")
}

/// A doc whose current winning revision is a tombstone: real CouchDB
/// reports this the same as a wholesale-missing doc on a plain `GET`
/// (no `rev=`/`open_revs=` requesting the tombstone explicitly) — the
/// distinguishing `reason` is what tells a client the id used to exist.
fn deleted() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not_found", "deleted")
}

fn internal_error() -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", "internal_error")
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(server_info))
        .route("/:db", put(create_db).get(db_info))
        .route("/:db/:id", get(get_doc).put(put_doc))
        .route("/:db/_bulk_docs", post(bulk_docs))
        .route("/:db/_revs_diff", post(revs_diff))
        .route("/:db/_bulk_get", post(bulk_get))
        .route("/:db/_local/:id", get(get_local).put(put_local))
        .route("/:db/_changes", get(changes))
        .layer(axum::middleware::from_fn_with_state(state.clone(), crate::auth::require_auth))
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
        return Err(ApiError(StatusCode::PRECONDITION_FAILED, "file_exists", "file_exists"));
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

#[derive(serde::Deserialize)]
struct GetDocParams {
    #[serde(default)]
    conflicts: bool,
}

async fn get_doc(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
    Query(params): Query<GetDocParams>,
) -> Result<Json<Value>, ApiError> {
    let db = Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    let tree = db.get_tree(&id).map_err(|_| internal_error())?;
    let tree = tree.ok_or_else(not_found)?;
    let winner = tree.winner().ok_or_else(not_found)?;
    let node = &tree.nodes[winner];
    if node.deleted {
        return Err(deleted());
    }
    let mut body = node.body.clone();
    body["_id"] = json!(id);
    body["_rev"] = json!(winner);
    if params.conflicts {
        let conflicts = tree.conflicts();
        if !conflicts.is_empty() {
            body["_conflicts"] = json!(conflicts);
        }
    }
    Ok(Json(body))
}

async fn put_doc(
    State(state): State<AppState>,
    Path((db_name, id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let db = Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    let feed = state.feeds.get_or_create(&db_name);
    let rev = write_doc(&db, &feed, &id, body).map_err(|_| internal_error())?;
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
    let feed = state.feeds.get_or_create(&db_name);
    let docs = payload
        .get("docs")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "bad_request", "bad_request"))?;
    let new_edits = payload.get("new_edits").and_then(Value::as_bool).unwrap_or(true);

    if new_edits {
        bulk_docs_new_edits_true(&db, &feed, docs)
    } else {
        bulk_docs_new_edits_false(&db, &feed, docs)
    }
}

fn bulk_docs_new_edits_true(db: &Db, feed: &ChangeFeed, docs: &[Value]) -> Result<Json<Value>, ApiError> {
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

        match write_doc(db, feed, &id, doc) {
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
fn bulk_docs_new_edits_false(db: &Db, feed: &ChangeFeed, docs: &[Value]) -> Result<Json<Value>, ApiError> {
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

        let write = || -> sled::Result<u64> {
            let mut tree = db.get_tree(id)?.unwrap_or_default();
            tree.insert_revision_chain(&chain, deleted, body);
            db.put_tree(id, &tree)
        };
        match write() {
            Ok(seq) => feed.publish(ChangeEvent { seq, doc_id: id.to_string() }),
            Err(_) => errors.push(json!({
                "id": id,
                "error": "internal_error",
                "reason": "failed to write document",
            })),
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
    let requested = payload.as_object().ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "bad_request", "bad_request"))?;

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
/// (plan §3). If `rev` is omitted, returns the current winner; deleted
/// docs/missing revs report as `error` entries rather than failing the
/// whole batch, matching CouchDB's per-item error shape.
async fn bulk_get(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let db = Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    let requests = payload
        .get("docs")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "bad_request", "bad_request"))?;

    let mut results = Vec::with_capacity(requests.len());
    for req in requests {
        let Some(id) = req.get("id").and_then(Value::as_str) else {
            results.push(json!({"id": Value::Null, "docs": [{"error": {"error": "bad_request", "reason": "missing id"}}]}));
            continue;
        };
        let requested_rev = req.get("rev").and_then(Value::as_str);

        let doc_result = (|| -> sled::Result<Value> {
            let Some(tree) = db.get_tree(id)? else {
                return Ok(json!({"error": {"id": id, "error": "not_found", "reason": "missing"}}));
            };
            let rev_id = match requested_rev {
                Some(rev) => rev.to_string(),
                None => match tree.winner() {
                    Some(rev) => rev.clone(),
                    None => return Ok(json!({"error": {"id": id, "error": "not_found", "reason": "missing"}})),
                },
            };
            match tree.nodes.get(&rev_id) {
                Some(node) if node.deleted => {
                    Ok(json!({"error": {"id": id, "rev": rev_id, "error": "not_found", "reason": "deleted"}}))
                }
                Some(node) => {
                    let mut body = node.body.clone();
                    body["_id"] = json!(id);
                    body["_rev"] = json!(rev_id);
                    Ok(json!({"ok": body}))
                }
                None => Ok(json!({"error": {"id": id, "rev": rev_id, "error": "not_found", "reason": "missing"}})),
            }
        })()
        .map_err(|_| internal_error())?;

        results.push(json!({"id": id, "docs": [doc_result]}));
    }
    Ok(Json(json!({"results": results})))
}

fn write_doc(db: &Db, feed: &ChangeFeed, id: &str, body: Value) -> sled::Result<String> {
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
    let seq = db.put_tree(id, &tree)?;
    feed.publish(ChangeEvent { seq, doc_id: id.to_string() });
    Ok(rev)
}

fn md5_like(body: &Value) -> u64 {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(body.to_string().as_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[0..8].try_into().unwrap())
}

#[derive(serde::Deserialize)]
struct ChangesParams {
    since: Option<String>,
    style: Option<String>,
    feed: Option<String>,
    timeout: Option<u64>,
}

/// `GET /{db}/_changes` — the core "what changed" feed (plan §3), in all
/// three delivery modes PouchDB's `live:true` sync relies on:
/// - normal: one JSON response with everything since `since`.
/// - `feed=longpoll`: holds the connection until something changes (or
///   `timeout` elapses), then responds exactly like normal mode.
/// - `feed=continuous`: streams newline-delimited JSON, catch-up rows
///   first, then live rows as writes happen.
async fn changes(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Query(params): Query<ChangesParams>,
) -> Result<Response, ApiError> {
    let db = Db::open(&state.root, &db_name).map_err(|_| internal_error())?;
    let since: u64 = params.since.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
    let style_all_docs = params.style.as_deref() == Some("all_docs");

    match params.feed.as_deref() {
        Some("continuous") => changes_continuous(state, db_name, db, since, style_all_docs),
        Some("longpoll") => changes_longpoll(state, db_name, db, since, style_all_docs, params.timeout).await,
        _ => changes_normal(&db, since, style_all_docs),
    }
}

/// Of possibly-many `(seq, doc_id)` log entries for the same doc, keep
/// only the highest seq — matches real CouchDB's `_changes`, which
/// reports one row per doc even if it changed multiple times in range.
fn dedupe_latest(entries: Vec<(u64, String)>) -> Vec<(u64, String)> {
    let mut latest: HashMap<String, u64> = HashMap::new();
    for (seq, id) in entries {
        latest.entry(id).and_modify(|s| *s = (*s).max(seq)).or_insert(seq);
    }
    let mut result: Vec<(u64, String)> = latest.into_iter().map(|(id, seq)| (seq, id)).collect();
    result.sort_by_key(|(seq, _)| *seq);
    result
}

fn build_change_row(db: &Db, seq: u64, doc_id: &str, style_all_docs: bool) -> Option<Value> {
    let tree = db.get_tree(doc_id).ok().flatten()?;
    let winner = tree.winner()?;
    let node = &tree.nodes[winner];
    let changes: Vec<Value> = if style_all_docs {
        tree.leaves().into_iter().map(|rev| json!({"rev": rev})).collect()
    } else {
        vec![json!({"rev": winner})]
    };
    let mut row = json!({"seq": seq, "id": doc_id, "changes": changes});
    if node.deleted {
        row["deleted"] = json!(true);
    }
    Some(row)
}

fn changes_normal(db: &Db, since: u64, style_all_docs: bool) -> Result<Response, ApiError> {
    let entries = db.changes_since(since).map_err(|_| internal_error())?;
    let results: Vec<Value> = dedupe_latest(entries)
        .iter()
        .filter_map(|(seq, id)| build_change_row(db, *seq, id, style_all_docs))
        .collect();
    let last_seq = db.current_seq().map_err(|_| internal_error())?;
    Ok(Json(json!({"results": results, "last_seq": last_seq})).into_response())
}

async fn changes_longpoll(
    state: AppState,
    db_name: String,
    db: Db,
    since: u64,
    style_all_docs: bool,
    timeout_ms: Option<u64>,
) -> Result<Response, ApiError> {
    let current = db.current_seq().map_err(|_| internal_error())?;
    if current <= since {
        let feed = state.feeds.get_or_create(&db_name);
        let mut rx = feed.subscribe();
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(60_000));
        // Whether we got woken by an event or timed out, re-derive from
        // storage rather than trusting the single event, so a burst of
        // writes coalesced between subscribe and recv isn't half-missed.
        let _ = tokio::time::timeout(timeout, rx.recv()).await;
    }
    changes_normal(&db, since, style_all_docs)
}

fn changes_continuous(state: AppState, db_name: String, db: Db, since: u64, style_all_docs: bool) -> Result<Response, ApiError> {
    let feed = state.feeds.get_or_create(&db_name);
    let start_seq = db.current_seq().map_err(|_| internal_error())?;
    let rx = feed.subscribe();

    let entries = db.changes_since(since).map_err(|_| internal_error())?;
    let historical: Vec<(u64, String)> =
        dedupe_latest(entries).into_iter().filter(|(seq, _)| *seq <= start_seq).collect();

    let db_hist = db.clone();
    let historical_lines: Vec<Result<Bytes, std::io::Error>> = historical
        .iter()
        .filter_map(|(seq, id)| build_change_row(&db_hist, *seq, id, style_all_docs))
        .map(|row| Ok(Bytes::from(format!("{row}\n"))))
        .collect();
    let historical_stream = futures::stream::iter(historical_lines);

    let db_live = db.clone();
    let live_stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(move |event| {
        let db_live = db_live.clone();
        async move {
            let event = event.ok()?;
            if event.seq <= start_seq {
                return None;
            }
            let row = build_change_row(&db_live, event.seq, &event.doc_id, style_all_docs)?;
            Some(Ok::<_, std::io::Error>(Bytes::from(format!("{row}\n"))))
        }
    });

    let stream = historical_stream.chain(live_stream);
    Response::builder()
        .header("Content-Type", "application/json")
        .body(Body::from_stream(stream))
        .map_err(|_| internal_error())
}
