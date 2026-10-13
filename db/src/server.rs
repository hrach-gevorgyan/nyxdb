//! The HTTP accept loop. `axum::serve` keeps every connection open for as
//! long as the client likes: before v0.1.8 an idle or half-sent
//! connection — even an unauthenticated one from anywhere on the LAN —
//! stayed open forever, and there was no cap on how many could be open.
//! This loop adds, per connection:
//!
//! - a header timeout: a request's headers must arrive in full within it
//!   (counted from accept, or from the first byte of a later request);
//! - an idle timeout: with no request in progress and no bytes either way
//!   for this long, a keep-alive connection is closed;
//! - TCP keepalive (SO_KEEPALIVE), so a peer that vanished (phone left the
//!   Wi-Fi) is noticed even on a connection waiting on a long request;
//! - a cap on open connections; more are closed as soon as accepted.
//!
//! A request in progress, including a response still streaming (a
//! longpoll with heartbeat, a continuous feed), is never cut by either
//! timeout; per-request limits are in `crate::limits`.

use axum::body::Body;
use axum::body::Bytes;
use http_body::Frame;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tower::Service;

#[derive(Clone, Copy, Debug)]
pub struct ConnectionLimits {
    pub header_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_connections: usize,
}

/// Activity of one connection, shared by its IO wrapper, its service
/// wrapper and its watcher. Times are milliseconds since `epoch`.
struct ConnState {
    epoch: Instant,
    in_flight: AtomicUsize,
    last_activity: AtomicU64,
    /// When the current request's headers started (0: not waiting for one).
    headers_since: AtomicU64,
}

impl ConnState {
    fn new() -> Self {
        // `headers_since` starts at accept: a new connection must send a
        // request within the header timeout. 1, not 0, since 0 means none.
        Self { epoch: Instant::now(), in_flight: AtomicUsize::new(0), last_activity: AtomicU64::new(1), headers_since: AtomicU64::new(1) }
    }

    fn now(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    fn on_read(&self) {
        let now = self.now();
        self.last_activity.store(now, Ordering::Relaxed);
        if self.in_flight.load(Ordering::Acquire) == 0 {
            let _ = self.headers_since.compare_exchange(0, now, Ordering::AcqRel, Ordering::Relaxed);
        }
    }

    fn on_write(&self) {
        self.last_activity.store(self.now(), Ordering::Relaxed);
    }

    fn request_started(&self) {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.headers_since.store(0, Ordering::Release);
    }

    fn request_finished(&self) {
        self.last_activity.store(self.now(), Ordering::Relaxed);
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }

    /// Why this connection should be closed now, if it should.
    fn expired(&self, limits: &ConnectionLimits) -> Option<&'static str> {
        if self.in_flight.load(Ordering::Acquire) > 0 {
            return None;
        }
        let now = self.now();
        let since = |t: u64| Duration::from_millis(now.saturating_sub(t));
        match self.headers_since.load(Ordering::Acquire) {
            0 if since(self.last_activity.load(Ordering::Relaxed)) > limits.idle_timeout => Some("idle"),
            0 => None,
            started if since(started) > limits.header_timeout => Some("headers not received in time"),
            _ => None,
        }
    }
}

/// The socket, recording every byte that moves.
struct TrackedIo {
    inner: TcpStream,
    state: Arc<ConnState>,
}

impl AsyncRead for TrackedIo {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.state.on_read();
        }
        polled
    }
}

impl AsyncWrite for TrackedIo {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write(cx, data);
        if matches!(polled, Poll::Ready(Ok(n)) if n > 0) {
            self.state.on_write();
        }
        polled
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Counts a request as in progress until its response body is finished
/// (or dropped): a streaming response is still a request in progress.
struct InFlight(Arc<ConnState>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.request_finished();
    }
}

struct TrackedBody {
    inner: Body,
    _in_flight: InFlight,
}

impl http_body::Body for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Turns on TCP keepalive: probes after 60 s of silence, so a connection
/// whose peer vanished is dropped by the OS instead of lingering.
fn set_keepalive(stream: &TcpStream) -> io::Result<()> {
    let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(60));
    #[cfg(any(windows, target_os = "linux", target_os = "macos", target_os = "android"))]
    let keepalive = keepalive.with_interval(Duration::from_secs(10));
    socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

/// Serves `app` on `listener` until `shutdown` resolves, then lets open
/// connections finish their current request (the caller bounds how long).
pub async fn serve<S>(listener: TcpListener, app: S, limits: ConnectionLimits, shutdown: impl Future<Output = ()>) -> io::Result<()>
where
    S: Service<axum::extract::Request, Response = axum::response::Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send,
{
    let slots = Arc::new(tokio::sync::Semaphore::new(limits.max_connections));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    // Per-connection errors (reset before accept, too many
                    // open files): log and keep serving.
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            tracing::warn!("refused a connection from {peer}: NYXDB_MAX_CONNECTIONS ({}) are open", limits.max_connections);
            drop(stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        if let Err(e) = set_keepalive(&stream) {
            tracing::warn!("can't enable TCP keepalive for {peer}: {e}");
        }
        let app = app.clone();
        let mut stop = stop_rx.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let state = Arc::new(ConnState::new());
            let io = TokioIo::new(TrackedIo { inner: stream, state: state.clone() });
            let svc_state = state.clone();
            let service = hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
                svc_state.request_started();
                let in_flight = InFlight(svc_state.clone());
                let mut app = app.clone();
                async move {
                    let response = app.call(request.map(Body::new)).await?;
                    Ok::<_, Infallible>(response.map(|inner| TrackedBody { inner, _in_flight: in_flight }))
                }
            });
            let conn = hyper::server::conn::http1::Builder::new().keep_alive(true).serve_connection(io, service);
            tokio::pin!(conn);
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    result = conn.as_mut() => {
                        if let Err(e) = result {
                            tracing::debug!("connection from {peer} ended: {e}");
                        }
                        break;
                    }
                    _ = tick.tick() => {
                        if let Some(reason) = state.expired(&limits) {
                            tracing::debug!("closing connection from {peer}: {reason}");
                            break; // dropping `conn` closes the socket
                        }
                    }
                    _ = stop.changed() => conn.as_mut().graceful_shutdown(),
                }
            }
        });
    }
    let _ = stop_tx.send(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepted_sockets_get_tcp_keepalive() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (stream, _) = listener.accept().await.unwrap();
        assert!(!socket2::SockRef::from(&stream).keepalive().unwrap());
        set_keepalive(&stream).unwrap();
        assert!(socket2::SockRef::from(&stream).keepalive().unwrap(), "SO_KEEPALIVE is set");
        drop(client.await.unwrap());
    }

    #[test]
    fn only_an_idle_connection_with_nothing_in_flight_expires() {
        let limits = ConnectionLimits { header_timeout: Duration::from_millis(50), idle_timeout: Duration::from_millis(100), max_connections: 1 };
        let state = ConnState::new();
        assert_eq!(state.expired(&limits), None);
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(state.expired(&limits), Some("headers not received in time"), "nothing sent since accept");

        state.request_started();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(state.expired(&limits), None, "a request in progress is never cut");
        state.request_finished();
        assert_eq!(state.expired(&limits), None);
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(state.expired(&limits), Some("idle"));

        state.on_read(); // the next request's first bytes
        assert_eq!(state.expired(&limits), None);
        std::thread::sleep(Duration::from_millis(70));
        assert_eq!(state.expired(&limits), Some("headers not received in time"));
    }
}
