use nyxdb::auth::Credentials;
use nyxdb::changes::ChangeFeedRegistry;
use nyxdb::routes::{build_router, AppState};
use std::path::Path;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::normalize_path::NormalizePathLayer;
use tower_http::trace::TraceLayer;

/// CORS only as broad as the actual client needs: `origins=*` is convenient but a real widening of attack
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
            .allow_credentials(true)
            // Browsers re-send a preflight before every cross-origin write
            // unless told they may cache it.
            .max_age(std::time::Duration::from_secs(3600)),
    )
}

fn env_number(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Err(_) => default,
        Ok(v) => v.parse().unwrap_or_else(|_| fail(EXIT_CONFIG, &format!("{name} must be a whole number, got {v:?}"))),
    }
}

/// Startup exit codes, so whatever launches NyxDB (Offlog runs it as a
/// sidecar) can tell failures apart without parsing text.
const EXIT_CONFIG: i32 = 2;
const EXIT_DATA: i32 = 3;
const EXIT_LISTEN: i32 = 4;

/// Reports a startup failure plainly — one line on stderr (and in the
/// log), a distinct exit code — instead of a panic message, a backtrace
/// hint, and exit code 101 for every kind of failure alike.
fn fail(code: i32, message: &str) -> ! {
    tracing::error!("{message}");
    eprintln!("nyxdb: {message}");
    std::process::exit(code);
}

#[tokio::main]
async fn main() {
    // `RUST_LOG` picks what is logged (default `info`); per-request lines
    // come from `tower_http` at debug: `RUST_LOG=info,tower_http=debug`.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

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
        match level.parse::<i32>() {
            Ok(level) if (1..=22).contains(&level) => config = config.compression_factor(level),
            _ => fail(EXIT_CONFIG, &format!("NYXDB_COMPRESSION_LEVEL must be an integer from 1 to 22, got {level:?}")),
        }
    }
    // sled's page cache defaults to 1 GB, so memory grew with the data the
    // server touched (measured with 100 MB of small docs: ~195 MB resident
    // after writing, ~163 MB after reading it back). sled's accounting
    // undercounts real memory by roughly 2x, so 32 MB of cache keeps the
    // same workload around ~105 MB (see test/memory). NYXDB_CACHE_MB to
    // tune. Attachment bytes are files and never enter this cache.
    let cache_mb: u64 = match std::env::var("NYXDB_CACHE_MB") {
        Err(_) => 32,
        Ok(v) => match v.parse() {
            Ok(mb) if mb > 0 => mb,
            _ => fail(EXIT_CONFIG, &format!("NYXDB_CACHE_MB must be a positive number of megabytes, got {v:?}")),
        },
    };
    config = config.cache_capacity(cache_mb * 1024 * 1024);
    let root = match config.open() {
        Ok(root) => Arc::new(root),
        Err(e) => fail(EXIT_DATA, &format!("can't open the data directory {data_dir:?}: {e}")),
    };
    let flush_root = root.clone();
    let server_uuid = nyxdb::storage::server_uuid(&root)
        .unwrap_or_else(|e| fail(EXIT_DATA, &format!("can't read or create the server id in {data_dir:?}: {e}")));

    let creds = Credentials::load_or_generate(Path::new(&data_dir))
        .unwrap_or_else(|e| fail(EXIT_DATA, &format!("can't read or create {data_dir}/credentials.json: {e}")));
    tracing::info!(
        "HTTP Basic auth required for all requests except GET / — username: {}, password: see {}/credentials.json (or NYXDB_USER/NYXDB_PASSWORD)",
        creds.username,
        data_dir
    );

    // Unbounded request bodies are a real memory-exhaustion vector (see
    // doc/AUDIT.md) — axum buffers the whole body before a Json
    // extractor ever runs. But PouchDB pushes 100 docs per _bulk_docs
    // request with attachments inline as base64, and the old 50 MB cap
    // turned an attachment-carrying batch into a 413 that replication
    // retried forever. 256 MB fits one maximal Offlog doc (ten 10 MB
    // attachments ≈ 133 MB as base64) with room to spare; clients sending
    // many attachment-heavy docs per batch should lower PouchDB's
    // `batch_size` to stay under it (doc/USAGE.md). Peak memory for a
    // request is a small multiple of its size. NYXDB_MAX_BODY_BYTES to
    // change it.
    let max_body_bytes: usize = match std::env::var("NYXDB_MAX_BODY_BYTES") {
        Err(_) => 256 * 1024 * 1024,
        Ok(v) => v
            .parse()
            .unwrap_or_else(|_| fail(EXIT_CONFIG, &format!("NYXDB_MAX_BODY_BYTES must be a number of bytes, got {v:?}"))),
    };

    // Attachment bytes are files under <data>/att (one folder per
    // database); a file younger than the grace period is never swept, as
    // its revision may not be committed yet. Tests shorten it.
    let attachment_grace = std::time::Duration::from_secs(env_number("NYXDB_ATTACHMENT_GRACE_SECS", 3600));
    let state = AppState {
        root,
        feeds: ChangeFeedRegistry::default(),
        creds: Arc::new(creds),
        server_uuid: Arc::from(server_uuid),
        locks: nyxdb::storage::WriteLocks::default(),
        att_root: Arc::new(Path::new(&data_dir).join("att")),
        attachment_grace,
    };
    let activity = Arc::new(nyxdb::maintenance::Activity::default());
    nyxdb::maintenance::spawn(state.clone(), activity.clone());
    let app = build_router(state).layer(axum::extract::DefaultBodyLimit::max(max_body_bytes));
    // Generous defaults: 300 s fits a 256 MB upload at under 1 MB/s, and
    // 512 requests in progress is far beyond what a few syncing devices
    // need. 0 turns the timeout off.
    let timeout_secs = env_number("NYXDB_REQUEST_TIMEOUT_SECS", 300);
    let slots = env_number("NYXDB_MAX_CONCURRENT_REQUESTS", 512);
    if slots == 0 {
        fail(EXIT_CONFIG, "NYXDB_MAX_CONCURRENT_REQUESTS must be at least 1");
    }
    let limits = nyxdb::limits::Limits {
        timeout: (timeout_secs > 0).then(|| std::time::Duration::from_secs(timeout_secs)),
        slots: Arc::new(tokio::sync::Semaphore::new(slots as usize)),
        activity,
    };
    let mut app = app.layer(axum::middleware::from_fn_with_state(limits, nyxdb::limits::limit_requests)).layer(TraceLayer::new_for_http());
    // CORS must wrap auth (outermost), not the other way round: tower_http's
    // CorsLayer answers OPTIONS preflight requests itself, and a preflight
    // never carries credentials — if auth ran first, every preflight would
    // 401 before CORS got a chance to respond.
    if let Some(cors) = cors_layer() {
        tracing::info!("CORS enabled: NYXDB_CORS_ORIGINS = {}", std::env::var("NYXDB_CORS_ORIGINS").unwrap_or_default());
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
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(e) => fail(EXIT_LISTEN, &format!("can't listen on {addr} (NYXDB_ADDR): {e}")),
    };
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
