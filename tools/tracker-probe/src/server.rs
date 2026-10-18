//! Tiny hyper HTTP/1 accept loop shared by `proxy` and `mock`.

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc, watch};

/// Maximum simultaneous client connections; excess connections wait in the
/// kernel backlog instead of exhausting file descriptors.
const MAX_CONNS: usize = 256;

/// Response type produced by handlers.
pub type Resp = Response<Full<Bytes>>;

/// Run an accept loop until `shutdown` flips to true, then drain in-flight
/// connections (up to 5 s).
pub async fn serve<H, Fut>(listener: TcpListener, handler: H, mut shutdown: watch::Receiver<bool>)
where
    H: Fn(Request<Incoming>, SocketAddr, SocketAddr) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Resp> + Send + 'static,
{
    let permits = Arc::new(Semaphore::new(MAX_CONNS));
    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    loop {
        let permit = tokio::select! {
            p = permits.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => break },
            _ = shutdown.changed() => break,
        };
        let (stream, remote) = tokio::select! {
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(_) => { tokio::time::sleep(Duration::from_millis(50)).await; continue; }
            },
            _ = shutdown.changed() => break,
        };
        let local = stream.local_addr().unwrap_or(remote);
        let _ = stream.set_nodelay(true);
        let handler = handler.clone();
        let mut sd = shutdown.clone();
        let done = done_tx.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _done = done;
            let svc = service_fn(move |req| {
                let fut = handler(req, remote, local);
                async move { Ok::<_, std::convert::Infallible>(fut.await) }
            });
            let mut builder = http1::Builder::new();
            builder.timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(15)).keep_alive(true);
            let conn = builder.serve_connection(TokioIo::new(stream), svc);
            tokio::pin!(conn);
            tokio::select! {
                _ = conn.as_mut() => {}
                _ = sd.changed() => {
                    conn.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(Duration::from_secs(5), conn.as_mut()).await;
                }
            }
        });
    }
    drop(done_tx);
    let _ = tokio::time::timeout(Duration::from_secs(6), done_rx.recv()).await;
}

/// Plain-text response helper.
pub fn text(status: u16, body: &str) -> Resp {
    let mut r = Response::new(Full::new(Bytes::from(body.to_owned())));
    *r.status_mut() = hyper::StatusCode::from_u16(status).unwrap_or(hyper::StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(hyper::header::CONTENT_TYPE, hyper::header::HeaderValue::from_static("text/plain"));
    r
}

/// Wait for Ctrl-C or SIGTERM, then flip the shutdown flag.
pub async fn shutdown_signal(tx: watch::Sender<bool>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    let _ = tx.send(true);
}
