use couchdb_clone::auth::Credentials;
use couchdb_clone::changes::ChangeFeedRegistry;
use couchdb_clone::routes::{build_router, AppState};
use std::path::Path;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::normalize_path::NormalizePathLayer;

/// CORS per plan §7: "only enable it as broadly as your actual client
/// needs" — `origins=*` is convenient but a real widening of attack
/// surface once the server is reachable beyond a trusted LAN. Default
/// is no CORS layer at all (same-origin only; doesn't affect non-browser
/// clients like PouchDB in Node, which isn't subject to CORS). Set
/// `COUCHDB_CLONE_CORS_ORIGINS` to a comma-separated allowlist (e.g. a
/// specific WebView origin) to enable it for a browser-based client.
fn cors_layer() -> Option<CorsLayer> {
    let origins = std::env::var("COUCHDB_CLONE_CORS_ORIGINS").ok()?;
    let parsed: Vec<axum::http::HeaderValue> = origins
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if parsed.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(parsed)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any),
    )
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let data_dir = std::env::var("COUCHDB_CLONE_DATA").unwrap_or_else(|_| "./data".into());
    let root = Arc::new(sled::open(&data_dir).expect("failed to open sled database"));

    let creds = Credentials::load_or_generate(Path::new(&data_dir)).expect("failed to load/generate credentials");
    tracing::info!(
        "HTTP Basic auth required for all requests except GET / — username: {}, password: see {}/credentials.json (or COUCHDB_CLONE_USER/COUCHDB_CLONE_PASSWORD)",
        creds.username,
        data_dir
    );

    let mut app = build_router(AppState { root, feeds: ChangeFeedRegistry::default(), creds: Arc::new(creds) });
    // CORS must wrap auth (outermost), not the other way round: tower_http's
    // CorsLayer answers OPTIONS preflight requests itself, and a preflight
    // never carries credentials — if auth ran first, every preflight would
    // 401 before CORS got a chance to respond.
    if let Some(cors) = cors_layer() {
        tracing::info!("CORS enabled: COUCHDB_CLONE_CORS_ORIGINS = {}", std::env::var("COUCHDB_CLONE_CORS_ORIGINS").unwrap());
        app = app.layer(cors);
    } else {
        tracing::info!("CORS disabled (same-origin only) — set COUCHDB_CLONE_CORS_ORIGINS to enable for a browser client");
    }
    // PouchDB's http adapter requests db/doc URLs with a trailing slash
    // (e.g. `GET /{db}/`); trim it so those still match our routes.
    let app = tower::ServiceBuilder::new()
        .layer(NormalizePathLayer::trim_trailing_slash())
        .service(app);
    let app = tower::make::Shared::new(app);

    let addr = std::env::var("COUCHDB_CLONE_ADDR").unwrap_or_else(|_| "127.0.0.1:5984".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind address");
    tracing::info!("listening on {addr}");
    axum::serve(listener, app).await.expect("server error");
}
