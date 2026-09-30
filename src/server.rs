//! Serving the application over plain HTTP or TLS, with one set of limits.
//!
//! Both paths run on `axum-server`, so they share what the plain path used to
//! lack: a deadline on graceful shutdown (set by whoever holds the
//! [`Handle`]), a time limit on reading a request's headers, and a cap on
//! concurrent connections. Without the last two, a client that opens sockets
//! and trickles a byte at a time holds each one — and its file descriptor —
//! for as long as it likes.

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

/// How long a client may take to send a request's headers. Generous for any
/// real client, short enough that a trickle of bytes cannot hold a socket.
/// It does not bound the body: a slow upload is governed by
/// `upload_idle_timeout_secs` instead.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The connection-level limits a server runs with.
#[derive(Debug, Clone, Copy)]
pub struct ServeLimits {
    /// Concurrent connections; `0` is unlimited. A connection over the cap is
    /// closed as soon as it is accepted.
    pub max_connections: usize,
    /// See [`HEADER_READ_TIMEOUT`].
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
    handle: Handle,
    limits: ServeLimits,
) -> Result<()> {
    let addr = config.socket_addr();
    let make_service = app.into_make_service_with_connect_info::<SocketAddr>();
    let acceptor = ConnectionLimit::new(DefaultAcceptor, limits.max_connections);

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
        let mut server = axum_server::bind(addr)
            .handle(handle)
            .acceptor(RustlsAcceptor::new(tls).acceptor(acceptor));
        limit_headers(server.http_builder(), limits);
        server.serve(make_service).await?;
    } else {
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
}

impl<A> ConnectionLimit<A> {
    /// Wrap `inner`, allowing `max` concurrent connections (`0` = no cap).
    pub fn new(inner: A, max: usize) -> Self {
        Self {
            inner,
            slots: (max > 0).then(|| Arc::new(Semaphore::new(max))),
        }
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
        let inner = self.inner.accept(stream, service);
        Box::pin(async move {
            let (stream, service) = inner.await?;
            Ok((
                Limited {
                    inner: stream,
                    _permit: permit,
                },
                service,
            ))
        })
    }
}

/// A connection that gives its [`ConnectionLimit`] slot back when dropped.
#[derive(Debug)]
pub struct Limited<S> {
    inner: S,
    _permit: Option<OwnedSemaphorePermit>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Limited<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
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
