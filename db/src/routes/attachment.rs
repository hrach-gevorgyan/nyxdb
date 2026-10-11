//! Standalone attachment endpoints: `GET/PUT/DELETE /{db}/{id}/{attname}`.

use super::*;

#[derive(serde::Deserialize)]
pub(super) struct AttachmentParams {
    rev: Option<String>,
}

/// `GET /{db}/{id}/{attname}` — fetch a single attachment's raw bytes
/// (not wrapped in JSON), with its stored `Content-Type`. `?rev=` picks
/// the branch: a pulling PouchDB fetches each conflicting branch's
/// attachments by that branch's rev.
pub(super) async fn get_attachment(
    State(state): State<AppState>,
    Path((db_name, id, attname)): Path<(String, String, String)>,
    Query(params): Query<AttachmentParams>,
) -> Result<Response, ApiError> {
    blocking(|| {
        let db = state.db(&db_name)?;
        let tree = db.get_tree(&id).map_err(|_| internal_error())?.ok_or_else(not_found)?;
        let (_, node) = resolve_rev(&tree, params.rev.as_deref())?;
        let stub = node.body.get("_attachments").and_then(|a| a.get(&attname)).ok_or_else(not_found)?;
        let digest = stub.get("digest").and_then(Value::as_str).ok_or_else(internal_error)?;
        let content_type =
            stub.get("content_type").and_then(Value::as_str).unwrap_or("application/octet-stream").to_string();
        let bytes = db.get_attachment(digest).map_err(|_| internal_error())?.ok_or_else(not_found)?;
        Response::builder().header(header::CONTENT_TYPE, content_type).body(Body::from(bytes)).map_err(|_| internal_error())
    })
}

/// `PUT /{db}/{id}/{attname}` — upload a single attachment directly:
/// the request body is the raw attachment bytes (not JSON), with
/// `Content-Type` giving its MIME type. Builds on the doc's current
/// winning revision, same last-write-wins semantics as `PUT /{db}/{id}`
/// (see the "no optimistic concurrency control" gap in `doc/USAGE.md` §7
/// — this endpoint doesn't enforce `?rev=` matching the current winner
/// either, for the same reason).
pub(super) async fn put_attachment(
    State(state): State<AppState>,
    Path((db_name, id, attname)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let db = state.db(&db_name)?;
    let feed = state.feeds.get_or_create(&db_name);
    let rev = blocking(|| {
        let _gate = db.attachment_writer();
        let mut doc_body = match db.get_tree(&id).map_err(|_| internal_error())? {
            Some(tree) => match tree.winner() {
                Some(winner) if !tree.nodes[winner].deleted => tree.nodes[winner].body.clone(),
                _ => json!({}),
            },
            None => json!({}),
        };
        let digest = crate::attachments::store_raw_attachment(&db, &body).map_err(|_| internal_error())?;
        if doc_body.get("_attachments").is_none() {
            doc_body["_attachments"] = json!({});
        }
        doc_body["_attachments"][&attname] =
            json!({"content_type": content_type, "digest": digest, "length": body.len(), "stub": true});
        write_doc(&db, &feed, &id, doc_body).map_err(|_| internal_error())
    })?;
    db.flush().await.map_err(|_| internal_error())?;
    Ok(Json(json!({"ok": true, "id": id, "rev": rev})))
}

/// `DELETE /{db}/{id}/{attname}` — remove a single attachment, creating
/// a new revision without it. The attachment's bytes stay in storage
/// (content-addressed, might be referenced by an earlier revision still
/// in the tree) — only the current body's reference to it is removed.
pub(super) async fn delete_attachment(
    State(state): State<AppState>,
    Path((db_name, id, attname)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let db = state.db(&db_name)?;
    let feed = state.feeds.get_or_create(&db_name);
    let rev = blocking(|| {
        let tree = db.get_tree(&id).map_err(|_| internal_error())?.ok_or_else(not_found)?;
        let winner = tree.winner().ok_or_else(not_found)?;
        if tree.nodes[winner].deleted {
            return Err(deleted());
        }
        let mut body = tree.nodes[winner].body.clone();
        let removed =
            body.get_mut("_attachments").and_then(Value::as_object_mut).and_then(|obj| obj.remove(&attname));
        if removed.is_none() {
            return Err(not_found());
        }
        write_doc(&db, &feed, &id, body).map_err(|_| internal_error())
    })?;
    db.flush().await.map_err(|_| internal_error())?;
    Ok(Json(json!({"ok": true, "id": id, "rev": rev})))
}
