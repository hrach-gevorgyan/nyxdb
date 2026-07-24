use couchdb_clone::auth::Credentials;
use couchdb_clone::changes::ChangeFeedRegistry;
use couchdb_clone::routes::{build_router, AppState};
use std::path::Path;
use std::sync::Arc;
use tower_http::normalize_path::NormalizePathLayer;

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

    let app = build_router(AppState { root, feeds: ChangeFeedRegistry::default(), creds: Arc::new(creds) });
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
