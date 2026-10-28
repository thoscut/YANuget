//! Serving the application over plain HTTP or TLS, with one set of limits.
//!
//! Both paths run on `axum-server`, so they share what the plain path used to
//! lack: a deadline on graceful shutdown (set by whoever holds the
//! [`Handle`]), a time limit on a connection's first bytes and on reading a
//! request's headers, and a cap on concurrent connections. Without the time
//! limits, a client that opens sockets and sends nothing, or trickles a byte at
//! a time, holds each one — its file descriptor and its connection slot — for
//! as long as it likes.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum_server::accept::{Accept, DefaultAcceptor};
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use axum_server::Handle;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::Config;
use crate::error::{Error, Result};

/// How long in-flight requests get to finish once shutdown starts, on either
/// path. After it, remaining connections are closed.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Request work that must not stop part-way: an indexing, an attach, a move
/// (see `web::helpers::detached`). It outlives its request when the client
/// disconnects, so the shutdown waits for it as it does for the background
/// tasks; dropped with the runtime, it would stop between two of its steps
/// after all.
pub fn detached_tasks() -> &'static tokio_util::task::TaskTracker {
    static TASKS: std::sync::OnceLock<tokio_util::task::TaskTracker> = std::sync::OnceLock::new();
    TASKS.get_or_init(tokio_util::task::TaskTracker::new)
}

/// How long a client may take to send a request's headers. Generous for any
/// real client, short enough that a trickle of bytes cannot hold a socket.
/// It does not bound the body: a slow upload is governed by
/// `upload_idle_timeout_secs` instead.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How often an idle HTTP/2 connection is pinged, and how long the answer may
/// take before the connection is closed. HTTP/2 has no header timeout between
/// requests, so this is what frees the slot of a peer that went away.
const HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(60);
const HTTP2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// The connection-level limits a server runs with.
#[derive(Debug, Clone, Copy)]
pub struct ServeLimits {
    /// Concurrent connections; `0` is unlimited. A connection over the cap is
    /// closed as soon as it is accepted.
    pub max_connections: usize,
    /// See [`HEADER_READ_TIMEOUT`]. The same limit applies to a new
    /// connection's first bytes (after the TLS handshake, with TLS on): until
    /// they arrive, hyper cannot tell HTTP/1 from HTTP/2, and neither
    /// protocol's own timeout has started.
    pub header_read_timeout: Duration,
}

impl ServeLimits {
    /// The limits `config` asks for.
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_connections: config.max_connections,
            header_read_timeout: HEADER_READ_TIMEOUT,
        }
    }
}

/// Serve `app` on the configured address until `handle` shuts it down.
///
/// With TLS on, the certificate is resolved (and a self-signed one generated
/// if needed) first. The caller learns the bound address from
/// [`Handle::listening`], and starts shutdown with
/// [`Handle::graceful_shutdown`], typically with [`SHUTDOWN_GRACE`].
pub async fn serve(
    app: axum::Router,
    config: &Config,
    handle: Handle<SocketAddr>,
    limits: ServeLimits,
) -> Result<()> {
    let addr = config.socket_addr();
    let make_service = app.into_make_service_with_connect_info::<SocketAddr>();

    if config.tls_enabled {
        // The process-wide provider; a second install (tests, or a second
        // server in one process) finds it already there, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let sans = crate::tls::certificate_sans(config.base_url.as_deref());
        let paths =
            crate::tls::ensure_certificate(config.tls_pair(), &config.data_dir, &sans).await?;
        let tls = RustlsConfig::from_pem_file(&paths.cert, &paths.key)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("failed to load TLS certificate: {e}")))?;
        // The limit wraps the TLS acceptor, so the first-bytes deadline covers
        // the handshake and the first request bytes after it alike.
        let acceptor = ConnectionLimit::new(RustlsAcceptor::new(tls), limits.max_connections)
            .first_bytes_timeout(limits.header_read_timeout);
        let mut server = axum_server::bind(addr).handle(handle).acceptor(acceptor);
        limit_headers(server.http_builder(), limits);
        server.serve(make_service).await?;
    } else {
        let acceptor = ConnectionLimit::new(DefaultAcceptor, limits.max_connections)
            .first_bytes_timeout(limits.header_read_timeout);
        let mut server = axum_server::bind(addr).handle(handle).acceptor(acceptor);
        limit_headers(server.http_builder(), limits);
        server.serve(make_service).await?;
    }
    Ok(())
}

fn limit_headers(builder: &mut Builder<TokioExecutor>, limits: ServeLimits) {
    // hyper only enforces the header timeout with a timer to measure it by.
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(HTTP2_KEEP_ALIVE_INTERVAL)
        .keep_alive_timeout(HTTP2_KEEP_ALIVE_TIMEOUT);
}

/// An acceptor that holds one permit per open connection and turns away
/// connections beyond the cap.
///
/// Refusing outright rather than queueing is deliberate: a queued connection
/// still holds its socket, which is the resource being protected.
#[derive(Debug, Clone)]
pub struct ConnectionLimit<A> {
    inner: A,
    slots: Option<Arc<Semaphore>>,
    first_bytes: Option<Duration>,
}

impl<A> ConnectionLimit<A> {
    /// Wrap `inner`, allowing `max` concurrent connections (`0` = no cap).
    pub fn new(inner: A, max: usize) -> Self {
        Self {
            inner,
            slots: (max > 0).then(|| Arc::new(Semaphore::new(max))),
            first_bytes: None,
        }
    }

    /// Close a connection that has not delivered a byte within `timeout` of
    /// being accepted, counting the time `inner` takes to accept it.
    pub fn first_bytes_timeout(mut self, timeout: Duration) -> Self {
        self.first_bytes = Some(timeout);
        self
    }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

impl<A, I, S> Accept<I, S> for ConnectionLimit<A>
where
    A: Accept<I, S>,
    A::Future: Send + 'static,
    A::Stream: Send + 'static,
    A::Service: Send + 'static,
{
    type Stream = Limited<A::Stream>;
    type Service = A::Service;
    type Future = BoxFuture<io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let permit = match &self.slots {
            None => None,
            Some(slots) => match slots.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    tracing::warn!("connection limit reached; closing a new connection");
                    // Dropping `stream` closes it.
                    return Box::pin(std::future::ready(Err(io::Error::other(
                        "connection limit reached",
                    ))));
                }
            },
        };
        let deadline = self
            .first_bytes
            .map(|timeout| Box::pin(tokio::time::sleep(timeout)));
        let inner = self.inner.accept(stream, service);
        Box::pin(async move {
            let (stream, service) = inner.await?;
            Ok((
                Limited {
                    inner: stream,
                    _permit: permit,
                    first_bytes: deadline,
                },
                service,
            ))
        })
    }
}

/// A connection that gives its [`ConnectionLimit`] slot back when dropped,
/// and fails its reads once the first-bytes deadline passes with nothing read.
#[derive(Debug)]
pub struct Limited<S> {
    inner: S,
    _permit: Option<OwnedSemaphorePermit>,
    /// Cleared by the first read that returns data.
    first_bytes: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Limited<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Some(deadline) = &mut self.first_bytes {
            match &polled {
                Poll::Ready(Ok(())) if buf.filled().len() > before => self.first_bytes = None,
                Poll::Pending if deadline.as_mut().poll(cx).is_ready() => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "no request within the header read timeout",
                    )));
                }
                _ => {}
            }
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Limited<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}
