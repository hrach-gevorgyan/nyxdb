//! HTTP Basic auth. Never ship "admin party" (no auth) as
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
    /// password.
    pub fn load_or_generate(data_dir: &Path) -> std::io::Result<Self> {
        if let (Ok(username), Ok(password)) =
            (std::env::var("NYXDB_USER"), std::env::var("NYXDB_PASSWORD"))
        {
            return Ok(Self { username, password });
        }

        // A file that exists but can't be read or parsed is an error, never
        // a reason to mint a new password: that silently locked out every
        // client holding the old one (before v0.1.8).
        let path = data_dir.join("credentials.json");
        match std::fs::read(&path) {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("it exists but can't be parsed ({e})"))
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        std::fs::create_dir_all(data_dir)?;
        let creds = Self { username: "admin".to_string(), password: uuid::Uuid::new_v4().to_string() };
        write_atomically(&path, &serde_json::to_vec_pretty(&creds)?)?;
        Ok(creds)
    }
}

/// Writes `bytes` to `path` so that `path` is either absent or complete,
/// even if the process dies midway: a temp file in the same folder,
/// fsynced, then renamed over `path`.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let temp = path.with_extension(format!("json.tmp-{}", uuid::Uuid::new_v4().simple()));
    let written = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        // A freshly-written file otherwise inherits the process umask —
        // often world-readable on Linux (the Docker deployment path).
        // No Windows equivalent via std; Windows ACLs default to the
        // owning user anyway for files created in a user-owned directory.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// `GET /` (server identification) is exempt — real CouchDB serves this
/// unauthenticated too, since clients probe it for feature detection
/// before they necessarily have credentials for a specific db.
pub async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request.uri().path() == "/" {
        return next.run(request).await;
    }

    if is_authorized(request.headers(), &state.creds) {
        next.run(request).await
    } else {
        unauthorized()
    }
}

/// Whether `headers` carry HTTP Basic credentials matching `creds`.
pub fn is_authorized(headers: &axum::http::HeaderMap, creds: &Credentials) -> bool {
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|encoded| base64::engine::general_purpose::STANDARD.decode(encoded).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok());
    match provided.as_deref().and_then(|decoded| decoded.split_once(':')) {
        Some((user, pass)) => constant_time_eq(user, &creds.username) && constant_time_eq(pass, &creds.password),
        None => false,
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
