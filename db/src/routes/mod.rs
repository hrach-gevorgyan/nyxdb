//! HTTP routes: the router, shared state and error type, and one module
//! per area of the API (see `build_router` for the full route table).

use crate::changes::{ChangeEvent, ChangeFeed, ChangeFeedRegistry};
use crate::revtree::{RevId, RevNode, RevTree};
use crate::storage::{Db, StorageResult, WriteLocks};
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

mod database;
mod documents;
mod attachment;
mod local;
mod replication;
mod changes_feed;

use database::*;
use documents::*;
use attachment::*;
use local::*;
use replication::*;
use changes_feed::*;

#[derive(Clone)]
pub struct AppState {
    pub root: Arc<sled::Db>,
    pub feeds: ChangeFeedRegistry,
    pub creds: Arc<crate::auth::Credentials>,
    /// Generated once at startup, stable for the process lifetime — real
    /// CouchDB returns the same uuid on every `GET /` for a given server,
    /// not a fresh one per request.
    pub server_uuid: Arc<str>,
    /// Per-database write locks — see `Db::update_tree`.
    pub locks: WriteLocks,
    /// `<data>/att`: one folder of attachment files per database.
    pub att_root: Arc<std::path::PathBuf>,
    /// An attachment file younger than this is never swept (it may belong
    /// to an upload whose revision isn't committed yet).
    pub attachment_grace: Duration,
}

impl AppState {
    /// Opens an existing database. Only `PUT /{db}` creates one; every
    /// other route on a db that doesn't exist gets CouchDB's 404 instead
    /// of silently creating it (a typo'd db name in a sync URL used to
    /// produce an empty db and a "successful" sync into it).
    fn db(&self, name: &str) -> Result<Db, ApiError> {
        blocking(|| {
            if !crate::storage::db_exists(&self.root, name) {
                return Err(db_not_found());
            }
            Db::open(&self.root, &self.att_root, name, &self.locks).map_err(|_| internal_error())
        })
    }
}

fn db_not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not_found", "Database does not exist.")
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
        .route("/:db", put(create_db).get(db_info).delete(delete_db))
        .route("/:db/:id", get(get_doc).put(put_doc).delete(delete_doc))
        .route("/:db/:id/:attname", get(get_attachment).put(put_attachment).delete(delete_attachment))
        .route("/:db/_design/:name", get(get_design).put(put_design).delete(delete_design))
        .route("/:db/_all_docs", get(all_docs).post(all_docs_keys))
        .route("/:db/_compact", post(compact))
        .route("/:db/_bulk_docs", post(bulk_docs))
        .route("/:db/_revs_diff", post(revs_diff))
        .route("/:db/_bulk_get", post(bulk_get))
        .route("/:db/_local/:id", get(get_local).put(put_local).delete(delete_local))
        .route("/:db/_changes", get(changes))
        .layer(axum::middleware::from_fn_with_state(state.clone(), crate::auth::require_auth))
        // Outermost: catches everything the handlers above don't produce
        // themselves — genuinely unmatched routes, malformed JSON bodies,
        // wrong/missing Content-Type. Axum's own responses for these are
        // plain-text or empty, which breaks JSON-only clients like
        // PouchDB (found by actually sending bad requests, see
        // doc/AUDIT.md).
        .layer(axum::middleware::from_fn(normalize_error_body))
        .with_state(state)
}

async fn normalize_error_body(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let already_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if already_json {
        return response;
    }

    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, 8192).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let reason = if text.trim().is_empty() {
        parts.status.canonical_reason().unwrap_or("error").to_lowercase().replace(' ', "_")
    } else {
        text.trim().to_string()
    };
    let error = match parts.status {
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        // CouchDB's name for an over-limit body (NYXDB_MAX_BODY_BYTES).
        StatusCode::PAYLOAD_TOO_LARGE => "too_large",
        s if s.is_server_error() => "internal_error",
        _ => "bad_request",
    };
    (parts.status, Json(json!({"error": error, "reason": reason}))).into_response()
}

async fn server_info(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "couchdb": "Welcome",
        "version": env!("CARGO_PKG_VERSION"),
        "uuid": &*state.server_uuid,
    }))
}

/// Runs storage work without stalling the async workers. sled calls block
/// the thread they run on; done directly in a handler they occupied one of
/// tokio's few async worker threads (one per CPU), so a handful of slow
/// requests — full `_changes` scans of a big db — left nothing to answer
/// even `GET /` (measured: 1.3–1.5 s). `block_in_place` tells the runtime
/// this worker is about to block, so its other tasks move to another
/// thread. Used in handlers only, never inside `Db`: it panics on the
/// single-threaded runtime unit tests use.
fn blocking<T>(work: impl FnOnce() -> T) -> T {
    tokio::task::block_in_place(work)
}
