//! Per-request limits applied around the whole router (see `main.rs`).

/// Per-request limits: at most `slots` requests in progress (more wait for
/// a slot), and each answered 408 if it takes longer than `timeout` —
/// including reading its body, so a stalled upload can't hold memory and
/// a slot forever. Authenticated `GET .../_changes` is exempt from both
/// (`exempt`): a longpoll legitimately waits (its own `timeout`, default
/// 60 s) and continuous feeds never end.
#[derive(Clone)]
pub struct Limits {
    pub timeout: Option<std::time::Duration>,
    pub slots: std::sync::Arc<tokio::sync::Semaphore>,
    /// Records each request, so background sweeps can wait for idle time.
    pub activity: std::sync::Arc<crate::maintenance::Activity>,
    /// To grant the `_changes` exemption only to authenticated requests.
    pub creds: std::sync::Arc<crate::auth::Credentials>,
}

/// Whether `request` may skip the request timeout and slot limit: only an
/// authenticated GET of a `_changes` feed. Before v0.1.8 the path alone
/// decided, so an unauthenticated request named `.../_changes` did too.
pub fn exempt(request: &axum::extract::Request, creds: &crate::auth::Credentials) -> bool {
    request.method() == axum::http::Method::GET
        && request.uri().path().ends_with("/_changes")
        && crate::auth::is_authorized(request.headers(), creds)
}

pub async fn limit_requests(
    axum::extract::State(limits): axum::extract::State<Limits>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    limits.activity.touch();
    if exempt(&request, &limits.creds) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Credentials;

    fn request(method: &str, path: &str, auth: Option<&str>) -> axum::extract::Request {
        let mut builder = axum::http::Request::builder().method(method).uri(path);
        if let Some(auth) = auth {
            builder = builder.header("Authorization", auth);
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    /// Only an authenticated GET of a feed may skip the request timeout
    /// and slot limit; before v0.1.8 any request whose path ended in
    /// `/_changes` did, authenticated or not.
    #[test]
    fn only_authenticated_changes_gets_are_exempt() {
        let creds = Credentials { username: "u".into(), password: "p".into() };
        let good = "Basic dTpw"; // u:p
        let bad = "Basic dTp4"; // u:x
        assert!(exempt(&request("GET", "/db/_changes", Some(good)), &creds));
        assert!(!exempt(&request("GET", "/db/_changes", None), &creds), "unauthenticated");
        assert!(!exempt(&request("GET", "/db/_changes", Some(bad)), &creds), "wrong password");
        assert!(!exempt(&request("POST", "/db/_changes", Some(good)), &creds), "not a GET");
        assert!(!exempt(&request("GET", "/db/doc", Some(good)), &creds), "not a feed");
    }
}
