use couchdb_clone::routes::{build_router, AppState};
use std::sync::Arc;
use tower_http::normalize_path::NormalizePathLayer;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let data_dir = std::env::var("COUCHDB_CLONE_DATA").unwrap_or_else(|_| "./data".into());
    let root = Arc::new(sled::open(&data_dir).expect("failed to open sled database"));

    let app = build_router(AppState { root });
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
