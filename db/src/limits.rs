//! Per-request limits applied around the whole router (see `main.rs`).

/// Per-request limits: at most `slots` requests in progress (more wait for
/// a slot), and each answered 408 if it takes longer than `timeout` —
/// including reading its body, so a stalled upload can't hold memory and
/// a slot forever. `_changes` is exempt from both: a longpoll legitimately
/// waits (its own `timeout`, default 60 s) and continuous feeds never end.
#[derive(Clone)]
pub struct Limits {
    pub timeout: Option<std::time::Duration>,
    pub slots: std::sync::Arc<tokio::sync::Semaphore>,
}

pub async fn limit_requests(
    axum::extract::State(limits): axum::extract::State<Limits>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if request.uri().path().ends_with("/_changes") {
        return next.run(request).await;
    }
    let _slot = limits.slots.acquire().await;
    let Some(timeout) = limits.timeout else { return next.run(request).await };
    match tokio::time::timeout(timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => (
            axum::http::StatusCode::REQUEST_TIMEOUT,
            axum::Json(serde_json::json!({"error": "request_timeout", "reason": "request took longer than NYXDB_REQUEST_TIMEOUT_SECS"})),
        )
            .into_response(),
    }
}
