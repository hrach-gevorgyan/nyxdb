//! HTTP Basic auth (plan §5, §7). Never ship "admin party" (no auth) as
//! a default — CouchDB itself does this out of the box and it's a
//! well-known footgun.

use crate::routes::AppState;
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fmt;
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Manual `Debug` impl that never prints the password — not exploited
/// anywhere today (nothing logs `Credentials`/`AppState` via `{:?}`),
/// but a derived `Debug` is a standing invitation for a future debug
/// log line to leak it. Found in the project audit, `doc/AUDIT.md`.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials").field("username", &self.username).field("password", &"<redacted>").finish()
    }
}

impl Credentials {
    /// `NYXDB_USER`/`NYXDB_PASSWORD` take priority (used
    /// by tests, and by anyone who wants to pin credentials explicitly).
    /// Otherwise, load a previously generated credentials file, or
    /// generate one now — random per-install, not a shared default
    /// password, matching plan §7's baseline.
    pub fn load_or_generate(data_dir: &Path) -> std::io::Result<Self> {
        if let (Ok(username), Ok(password)) =
            (std::env::var("NYXDB_USER"), std::env::var("NYXDB_PASSWORD"))
        {
            return Ok(Self { username, password });
        }

        let path = data_dir.join("credentials.json");
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(creds) = serde_json::from_slice(&bytes) {
                return Ok(creds);
            }
        }

        std::fs::create_dir_all(data_dir)?;
        let creds = Self { username: "admin".to_string(), password: uuid::Uuid::new_v4().to_string() };
        std::fs::write(&path, serde_json::to_vec_pretty(&creds)?)?;
        // A freshly-written file otherwise inherits the process umask —
        // often world-readable on Linux (the Docker deployment path).
        // No Windows equivalent via std; Windows ACLs default to the
        // owning user anyway for files created in a user-owned directory.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(creds)
    }
}

/// `GET /` (server identification) is exempt — real CouchDB serves this
/// unauthenticated too, since clients probe it for feature detection
/// before they necessarily have credentials for a specific db.
pub async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request.uri().path() == "/" {
        return next.run(request).await;
    }

    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|encoded| base64::engine::general_purpose::STANDARD.decode(encoded).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|decoded| decoded.split_once(':').map(|(u, p)| (u.to_string(), p.to_string())));

    match provided {
        Some((user, pass))
            if constant_time_eq(&user, &state.creds.username) && constant_time_eq(&pass, &state.creds.password) =>
        {
            next.run(request).await
        }
        _ => unauthorized(),
    }
}

/// Plain `==` on credentials is a timing side-channel — comparison
/// exits early on the first mismatched byte, so response time leaks
/// how many leading characters an attacker guessed correctly. Lower
/// real-world severity here since HTTP Basic auth already sends
/// credentials in cleartext over plain HTTP, but cheap to close.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes().iter().zip(b.as_bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"nyxdb\"")],
        Json(json!({"error": "unauthorized", "reason": "Name or password is incorrect."})),
    )
        .into_response()
}
