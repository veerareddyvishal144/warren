//! The byte tunnel behind an upgraded connection (WebSocket, the ttyd
//! terminal): both halves are copied until either side closes, and the idle
//! timer is kept honest by reporting activity while bytes actually flow.

use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use warren_core::Lifecycle;

/// Wait for both upgrades, then copy bytes both ways until the tunnel closes.
pub(crate) async fn run(
    client: OnUpgrade,
    upstream: OnUpgrade,
    lifecycle: Arc<dyn Lifecycle>,
    id: String,
    heartbeat: Duration,
) {
    let (client, upstream) = match tokio::try_join!(client, upstream) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(instance = %id, err = %e, "upgrade handshake failed");
            return;
        }
    };
    let moved = Arc::new(AtomicBool::new(false));
    let mut client = Activity::new(TokioIo::new(client), moved.clone());
    let mut upstream = Activity::new(TokioIo::new(upstream), moved.clone());
    tracing::debug!(instance = %id, "tunnel open");

    let copy = tokio::io::copy_bidirectional(&mut client, &mut upstream);
    tokio::pin!(copy);
    let mut beat = tokio::time::interval(heartbeat.max(Duration::from_millis(10)));
    beat.tick().await; // the first tick completes immediately
    loop {
        tokio::select! {
            result = &mut copy => {
                match result {
                    Ok((up, down)) => tracing::debug!(instance = %id, up, down, "tunnel closed"),
                    Err(e) => tracing::debug!(instance = %id, err = %e, "tunnel ended with an error"),
                }
                break;
            }
            _ = beat.tick() => {
                if moved.swap(false, Ordering::Relaxed) {
                    lifecycle.record_activity(&id);
                }
            }
        }
    }
}

/// An IO wrapper that raises a flag whenever bytes move through it.
struct Activity<T> {
    inner: T,
    moved: Arc<AtomicBool>,
}

impl<T> Activity<T> {
    fn new(inner: T, moved: Arc<AtomicBool>) -> Self {
        Activity { inner, moved }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Activity<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.moved.store(true, Ordering::Relaxed);
        }
        result
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Activity<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, data);
        if matches!(result, Poll::Ready(Ok(n)) if n > 0) {
            self.moved.store(true, Ordering::Relaxed);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
