use nyxdb::auth::Credentials;
use nyxdb::changes::ChangeFeedRegistry;
use nyxdb::routes::{build_router, AppState};
use std::path::Path;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::normalize_path::NormalizePathLayer;

/// CORS per plan §7: "only enable it as broadly as your actual client
/// needs" — `origins=*` is convenient but a real widening of attack
/// surface once the server is reachable beyond a trusted LAN. Default
/// is no CORS layer at all (same-origin only; doesn't affect non-browser
/// clients like PouchDB in Node, which isn't subject to CORS). Set
/// `NYXDB_CORS_ORIGINS` to a comma-separated allowlist (e.g. a
/// specific WebView origin) to enable it for a browser-based client.
fn cors_layer() -> Option<CorsLayer> {
    let origins = std::env::var("NYXDB_CORS_ORIGINS").ok()?;
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
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::PUT,
                axum::http::Method::DELETE,
                axum::http::Method::OPTIONS,
                axum::http::Method::HEAD,
            ])
            .allow_headers([axum::http::header::CONTENT_TYPE, axum::http::header::AUTHORIZATION])
            .allow_credentials(true),
    )
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let data_dir = std::env::var("NYXDB_DATA").unwrap_or_else(|_| "./data".into());
    // zstd compression is off by default in sled; JSON document bodies
    // compress well, and enabling it was one of two changes (the other
    // being storage.rs's use of generate_id() instead of a hand-rolled
    // counter write) that closed most of the on-disk-size gap found in
    // doc/BENCHMARKS.md. Level defaults to sled's own default (5);
    // override via NYXDB_COMPRESSION_LEVEL (1-22, lower = faster
    // but less compression) — exposed as an env var specifically to A/B
    // this trade-off without rebuilding, per doc/BENCHMARKS.md's
    // speed-vs-size experiment.
    let mut config = sled::Config::new().path(&data_dir).use_compression(true);
    if let Ok(level) = std::env::var("NYXDB_COMPRESSION_LEVEL") {
        let level: i32 = level.parse().expect("NYXDB_COMPRESSION_LEVEL must be an integer 1-22");
        config = config.compression_factor(level);
    }
    let root = Arc::new(config.open().expect("failed to open sled database"));
    let flush_root = root.clone();
    let server_uuid = nyxdb::storage::server_uuid(&root).expect("failed to read or create the server uuid");

    let creds = Credentials::load_or_generate(Path::new(&data_dir)).expect("failed to load/generate credentials");
    tracing::info!(
        "HTTP Basic auth required for all requests except GET / — username: {}, password: see {}/credentials.json (or NYXDB_USER/NYXDB_PASSWORD)",
        creds.username,
        data_dir
    );

    // Unbounded request bodies are a real memory-exhaustion vector (see
    // doc/AUDIT.md) — axum buffers the whole body before a Json
    // extractor ever runs. Default is generous enough for a large
    // _bulk_docs sync batch without being effectively unlimited;
    // override via NYXDB_MAX_BODY_BYTES if a deployment needs
    // something different.
    let max_body_bytes: usize = std::env::var("NYXDB_MAX_BODY_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50 * 1024 * 1024);

    let mut app = build_router(AppState {
        root,
        feeds: ChangeFeedRegistry::default(),
        creds: Arc::new(creds),
        server_uuid: Arc::from(server_uuid),
        locks: nyxdb::storage::WriteLocks::default(),
    })
        .layer(axum::extract::DefaultBodyLimit::max(max_body_bytes));
    // CORS must wrap auth (outermost), not the other way round: tower_http's
    // CorsLayer answers OPTIONS preflight requests itself, and a preflight
    // never carries credentials — if auth ran first, every preflight would
    // 401 before CORS got a chance to respond.
    if let Some(cors) = cors_layer() {
        tracing::info!("CORS enabled: NYXDB_CORS_ORIGINS = {}", std::env::var("NYXDB_CORS_ORIGINS").unwrap());
        app = app.layer(cors);
    } else {
        tracing::info!("CORS disabled (same-origin only) — set NYXDB_CORS_ORIGINS to enable for a browser client");
    }
    // PouchDB's http adapter requests db/doc URLs with a trailing slash
    // (e.g. `GET /{db}/`); trim it so those still match our routes.
    let app = tower::ServiceBuilder::new()
        .layer(NormalizePathLayer::trim_trailing_slash())
        .service(app);
    let app = tower::make::Shared::new(app);

    let addr = std::env::var("NYXDB_ADDR").unwrap_or_else(|_| "127.0.0.1:5984".into());
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind address");
    tracing::info!("listening on {addr}");

    // Every write is already flushed before it's acknowledged (handlers
    // call `Db::flush`), so a hard kill — which is how Offlog stops this
    // process — loses nothing acknowledged. A clean stop flushes once more
    // anyway. Live `feed=continuous` clients never close their connection
    // on their own, so instead of waiting on them indefinitely, flush as
    // soon as the signal arrives and give connections a short grace period.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async {
        let _ = stop_rx.await;
    });
    let server = tokio::spawn(async move { server.await });
    shutdown_signal().await;
    tracing::info!("shutdown requested: flushing to disk");
    if let Err(e) = flush_root.flush_async().await {
        tracing::error!("flush on shutdown failed: {e}");
    }
    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), server).await;
    if let Err(e) = flush_root.flush_async().await {
        tracing::error!("final flush failed: {e}");
    }
    tracing::info!("stopped");
}

/// Ctrl-C everywhere; on Windows also Ctrl-Break, console close, and
/// system shutdown; on Unix also SIGTERM. TerminateProcess/SIGKILL can't
/// be caught — that case is covered by flushing before every ack.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(windows)]
    let platform = async {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};
        match (ctrl_break(), ctrl_close(), ctrl_shutdown()) {
            (Ok(mut brk), Ok(mut close), Ok(mut shutdown)) => {
                tokio::select! {
                    _ = brk.recv() => {}
                    _ = close.recv() => {}
                    _ = shutdown.recv() => {}
                }
            }
            _ => std::future::pending::<()>().await,
        }
    };
    #[cfg(unix)]
    let platform = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = platform => {}
    }
}
