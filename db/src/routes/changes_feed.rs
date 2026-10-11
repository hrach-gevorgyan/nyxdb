//! `GET /{db}/_changes`: normal, longpoll and continuous feeds.

use super::*;

#[derive(serde::Deserialize)]
pub(super) struct ChangesParams {
    since: Option<String>,
    style: Option<String>,
    feed: Option<String>,
    timeout: Option<u64>,
    /// Milliseconds, or `true` for CouchDB's default of 60 s.
    heartbeat: Option<String>,
    /// At most this many rows; `last_seq` is then the last returned row's seq.
    limit: Option<usize>,
}

/// `heartbeat` as a duration: a number of ms, or `true` for 60 s (CouchDB's
/// default). Anything else — absent, `false`, 0, garbage — means none.
pub(super) fn heartbeat_of(params: &ChangesParams) -> Option<Duration> {
    match params.heartbeat.as_deref()? {
        "true" => Some(Duration::from_secs(60)),
        ms => ms.parse::<u64>().ok().filter(|ms| *ms > 0).map(Duration::from_millis),
    }
}

/// `GET /{db}/_changes` — the core "what changed" feed, in all
/// three delivery modes PouchDB's `live:true` sync relies on:
/// - normal: one JSON response with everything since `since`.
/// - `feed=longpoll`: holds the connection until something changes (or
///   `timeout` elapses), then responds exactly like normal mode.
/// - `feed=continuous`: streams newline-delimited JSON, catch-up rows
///   first, then live rows as writes happen.
pub(super) async fn changes(
    State(state): State<AppState>,
    Path(db_name): Path<String>,
    Query(params): Query<ChangesParams>,
) -> Result<Response, ApiError> {
    let db = state.db(&db_name)?;
    let since: u64 = params.since.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
    let style_all_docs = params.style.as_deref() == Some("all_docs");

    let heartbeat = heartbeat_of(&params);
    match params.feed.as_deref() {
        Some("continuous") => blocking(|| changes_continuous(state, db_name, db, since, style_all_docs, heartbeat)),
        Some("longpoll") => changes_longpoll(state.feeds.get_or_create(&db_name), db, since, style_all_docs, params.limit, params.timeout, heartbeat).await,
        _ => blocking(|| changes_normal(&db, since, style_all_docs, params.limit)),
    }
}

/// Of possibly-many `(seq, doc_id)` log entries for the same doc, keep
/// only the highest seq — matches real CouchDB's `_changes`, which
/// reports one row per doc even if it changed multiple times in range.
pub(super) fn dedupe_latest(entries: Vec<(u64, String)>) -> Vec<(u64, String)> {
    let mut latest: HashMap<String, u64> = HashMap::new();
    for (seq, id) in entries {
        latest.entry(id).and_modify(|s| *s = (*s).max(seq)).or_insert(seq);
    }
    let mut result: Vec<(u64, String)> = latest.into_iter().map(|(id, seq)| (seq, id)).collect();
    result.sort_by_key(|(seq, _)| *seq);
    result
}

/// One `_changes` row for `doc_id`, or `None` if the doc is gone (its
/// database was deleted under the feed). A tree that can't be read is an
/// error, never a skipped row: skipping let the client's checkpoint pass a
/// change it never received.
pub(super) fn build_change_row(db: &Db, seq: u64, doc_id: &str, style_all_docs: bool) -> Result<Option<Value>, ApiError> {
    let tree = db.get_tree(doc_id).map_err(|err| {
        tracing::error!("_changes: cannot read doc {doc_id:?}: {err}");
        internal_error()
    })?;
    let Some(tree) = tree else { return Ok(None) };
    let Some(winner) = tree.winner() else { return Ok(None) };
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
    Ok(Some(row))
}

pub(super) fn changes_normal(db: &Db, since: u64, style_all_docs: bool, limit: Option<usize>) -> Result<Response, ApiError> {
    Ok(Json(changes_body(db, since, style_all_docs, limit)?).into_response())
}

pub(super) fn changes_body(db: &Db, since: u64, style_all_docs: bool, limit: Option<usize>) -> Result<Value, ApiError> {
    let entries = db.changes_since(since).map_err(|_| internal_error())?;
    // `last_seq` is the highest seq this scan actually covered (or
    // `since` if it found nothing) — never the db's current seq read
    // afterwards. A write landing between the scan and that read would
    // otherwise sit below the client's new checkpoint without ever having
    // been returned, and be skipped forever (see doc/changelog.md, v0.1.7).
    let mut last_seq = entries.iter().map(|(seq, _)| *seq).max().unwrap_or(since);
    let results: Vec<Value> = dedupe_latest(entries)
        .iter()
        .filter_map(|(seq, id)| build_change_row(db, *seq, id, style_all_docs).transpose())
        .take(limit.unwrap_or(usize::MAX))
        .collect::<Result<_, _>>()?;
    // With a `limit` that cut the feed short, the checkpoint must stop at
    // the last row actually returned so the next page resumes from there.
    if let Some(last) = results.last().filter(|_| limit.is_some_and(|n| n <= results.len())) {
        last_seq = last["seq"].as_u64().unwrap_or(last_seq);
    }
    Ok(json!({"results": results, "last_seq": last_seq}))
}

pub(super) async fn changes_longpoll(
    feed: ChangeFeed,
    db: Db,
    since: u64,
    style_all_docs: bool,
    limit: Option<usize>,
    timeout_ms: Option<u64>,
    heartbeat: Option<Duration>,
) -> Result<Response, ApiError> {
    let Some(heartbeat) = heartbeat else {
        wait_for_change(&db, &feed, since, Duration::from_millis(timeout_ms.unwrap_or(60_000)), || {}).await;
        return blocking(|| changes_normal(&db, since, style_all_docs, limit));
    };
    // With a heartbeat, CouchDB waits as long as it takes (the heartbeat
    // overrides `timeout`) and writes a newline every `heartbeat` so the
    // client (PouchDB aborts after heartbeat + 5 s of silence) and any
    // proxy keep the request open. The final body is ordinary JSON; JSON
    // parsers skip the leading newlines.
    let stream = futures::stream::unfold(Some((db, feed)), move |state| async move {
        let (db, feed) = state?;
        let line = if wait_for_change(&db, &feed, since, heartbeat, || {}).await {
            match blocking(|| changes_body(&db, since, style_all_docs, limit)) {
                Ok(body) => return Some((Ok::<_, std::io::Error>(Bytes::from(body.to_string())), None)),
                Err(_) => return None,
            }
        } else {
            Bytes::from_static(b"\n")
        };
        Some((Ok(line), Some((db, feed))))
    });
    Response::builder()
        .header("Content-Type", "application/json")
        .body(Body::from_stream(stream))
        .map_err(|_| internal_error())
}

/// Waits until `db` has a change after `since`, or `wait` elapses.
///
/// Subscribes *before* checking storage: a write that lands after the
/// check is then still delivered to the subscription. The other way
/// round, a write between "check storage (nothing)" and "subscribe" was
/// missed, and the client sat out the whole timeout (60 s by default)
/// for a change that had already happened.
///
/// `between` runs right after the storage check — a no-op in
/// production; the test uses it to land a write in exactly that gap.
pub(super) async fn wait_for_change(db: &Db, feed: &ChangeFeed, since: u64, wait: Duration, between: impl FnOnce()) -> bool {
    let mut rx = feed.subscribe();
    if db.current_seq().map(|seq| seq > since).unwrap_or(true) {
        return true;
    }
    between();
    tokio::time::timeout(wait, rx.recv()).await.is_ok()
}

/// Streaming state for `feed=continuous`. `queue` holds rows not yet
/// sent (catch-up rows initially, then re-catch-up rows after a lag
/// recovery); `last_seq` is the highest seq actually sent so far.
pub(super) struct ContinuousState {
    db: Db,
    rx: tokio::sync::broadcast::Receiver<ChangeEvent>,
    queue: std::collections::VecDeque<(u64, String)>,
    last_seq: u64,
    style_all_docs: bool,
    /// Write a newline after this much silence, so the client and any
    /// proxy keep an idle feed open.
    heartbeat: Option<Duration>,
}

pub(super) fn changes_continuous(
    state: AppState,
    db_name: String,
    db: Db,
    since: u64,
    style_all_docs: bool,
    heartbeat: Option<Duration>,
) -> Result<Response, ApiError> {
    let feed = state.feeds.get_or_create(&db_name);
    let rx = feed.subscribe();

    let entries = db.changes_since(since).map_err(|_| internal_error())?;
    let historical = dedupe_latest(entries);
    let last_seq = historical.last().map(|(seq, _)| *seq).unwrap_or(since);

    let initial =
        ContinuousState { db, rx, queue: historical.into_iter().collect(), last_seq, style_all_docs, heartbeat };

    // A plain broadcast subscription silently drops messages once a slow
    // receiver falls behind the channel's fixed capacity (`RecvError::
    // Lagged`) — under load (many docs written faster than a subscriber
    // reads), that means permanently missing changes rather than an
    // occasional catch-up. Recovering by re-querying storage from
    // `last_seq` (instead of just resubscribing) is what makes this
    // correct rather than merely fast in the common case.
    let stream = futures::stream::unfold(initial, |mut st| async move {
        loop {
            if let Some((seq, id)) = st.queue.pop_front() {
                match blocking(|| build_change_row(&st.db, seq, &id, st.style_all_docs)) {
                    Ok(Some(row)) => {
                        st.last_seq = st.last_seq.max(seq);
                        return Some((Ok::<_, std::io::Error>(Bytes::from(format!("{row}\n"))), st));
                    }
                    Ok(None) => continue,
                    // End the feed before the unreadable row: the client
                    // reconnects from the last seq it actually got.
                    Err(_) => return None,
                }
            }

            let received = match st.heartbeat {
                Some(heartbeat) => match tokio::time::timeout(heartbeat, st.rx.recv()).await {
                    Ok(received) => received,
                    Err(_) => return Some((Ok(Bytes::from_static(b"\n")), st)),
                },
                None => st.rx.recv().await,
            };
            match received {
                Ok(event) => {
                    if event.seq <= st.last_seq {
                        continue;
                    }
                    match blocking(|| build_change_row(&st.db, event.seq, &event.doc_id, st.style_all_docs)) {
                        Ok(Some(row)) => {
                            st.last_seq = event.seq;
                            return Some((Ok(Bytes::from(format!("{row}\n"))), st));
                        }
                        Ok(None) => continue,
                        Err(_) => return None,
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => match blocking(|| st.db.changes_since(st.last_seq)) {
                    Ok(missed) => {
                        st.queue = dedupe_latest(missed).into_iter().collect();
                        continue;
                    }
                    Err(_) => return None,
                },
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Response::builder()
        .header("Content-Type", "application/json")
        .body(Body::from_stream(stream))
        .map_err(|_| internal_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::WriteLocks;

    /// A doc whose stored tree can't be read must fail `_changes`, not
    /// vanish from it: a silently dropped row lets the client's checkpoint
    /// move past a change it never got.
    #[test]
    fn changes_reports_an_unreadable_doc_instead_of_dropping_it() {
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, "bad", &WriteLocks::default()).unwrap();
        let feed = ChangeFeed::default();
        for id in ["a", "b"] {
            db.update_tree(id, &feed, |tree| tree.insert_revision_chain(&["1-a".to_string()], false, json!({})))
                .unwrap();
        }
        db.docs.insert("a", &b"\xff\xff not a tree"[..]).unwrap();
        assert!(changes_body(&db, 0, false, None).is_err());
    }

    /// A write that lands after longpoll has checked storage (found
    /// nothing) but before it starts listening must still wake it — it
    /// used to be missed, and the client sat out the full timeout (60 s by
    /// default) for a change that had already happened.
    #[tokio::test]
    async fn longpoll_sees_a_write_landing_right_after_its_storage_check() {
        let base = sled::Config::new().temporary(true).open().unwrap();
        let db = Db::open(&base, "lp", &WriteLocks::default()).unwrap();
        let feed = ChangeFeed::default();
        let started = std::time::Instant::now();
        let woke = wait_for_change(&db, &feed, 0, Duration::from_secs(3), || {
            db.update_tree("doc", &feed, |tree| tree.insert_revision_chain(&["1-a".to_string()], false, json!({})))
                .unwrap();
        })
        .await;
        assert!(woke, "longpoll timed out instead of seeing the write");
        assert!(started.elapsed() < Duration::from_secs(1), "took {:?}", started.elapsed());
    }
}
