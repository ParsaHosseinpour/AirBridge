//! Cooperative cancellation for the batch layer.
//!
//! The FFI boundary already has a cancellable stream for single transfers.
//! This is the same primitive in the orchestration crate, kept independent of
//! the FFI types so `libcore_transfer` can be tested and reused without a
//! foreign runtime.
//!
//! Cancellation has to interrupt a socket that may never produce another byte.
//! Checking a flag between chunks would leave `cancel()` inert while a writer
//! is parked in `poll_write`. [`CancelStream`] therefore checks the token on
//! every poll and registers the task's waker; [`CancelToken::cancel`] wakes
//! that task, so the next poll returns an `Interrupted` I/O error and the
//! transfer unwinds through its normal cleanup path.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Debug, Default)]
struct TokenInner {
    cancelled: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
}

/// A cloneable cancellation flag with task wakeup.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    inner: Arc<TokenInner>,
}

impl CancelToken {
    /// Creates a token that has not been cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation and wakes the current waiter.
    ///
    /// Safe to call from any thread, any number of times. Never blocks.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        let wakers = std::mem::take(
            &mut *self
                .inner
                .wakers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for waker in wakers {
            waker.wake();
        }
    }

    /// Returns whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    fn register(&self, waker: &Waker) -> bool {
        let mut wakers = self
            .inner
            .wakers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(slot) = wakers.iter_mut().find(|stored| stored.will_wake(waker)) {
            *slot = waker.clone();
        } else {
            wakers.push(waker.clone());
        }
        self.is_cancelled()
    }

    /// Completes when cancellation is requested.
    ///
    /// The future registers the calling task's waker; the ordering argument is
    /// the same one documented on the FFI canceller: the poller re-reads the
    /// flag after publishing its waker, so either it observes the flag or a
    /// concurrent `cancel()` observes the waker. There is no missed wakeup.
    pub fn cancelled(&self) -> Cancelled<'_> {
        Cancelled { token: self }
    }
}

/// Future returned by [`CancelToken::cancelled`].
#[derive(Debug)]
pub struct Cancelled<'a> {
    token: &'a CancelToken,
}

impl Future for Cancelled<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.token.register(cx.waker()) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// Wraps a byte stream and fails every poll after cancellation.
#[derive(Debug)]
pub struct CancelStream<S> {
    inner: S,
    token: CancelToken,
}

impl<S> CancelStream<S> {
    /// Wraps `inner` with `token`.
    #[must_use]
    pub fn new(inner: S, token: CancelToken) -> Self {
        Self { inner, token }
    }

    /// Returns the wrapped stream and the token.
    #[must_use]
    pub fn into_inner(self) -> (S, CancelToken) {
        (self.inner, self.token)
    }

    /// A clone of the token, for a caller that needs to trigger cancellation.
    #[must_use]
    pub fn token(&self) -> CancelToken {
        self.token.clone()
    }
}

impl<S> AsyncRead for CancelStream<S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.token.register(cx.waker()) {
            return Poll::Ready(Err(cancelled_error()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S> AsyncWrite for CancelStream<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.token.register(cx.waker()) {
            return Poll::Ready(Err(cancelled_error()));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.token.register(cx.waker()) {
            return Poll::Ready(Err(cancelled_error()));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Shutdown is the cleanup path; let it close the socket even after the
        // token flips, otherwise a cancelled batch would leak its TCP handle.
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

fn cancelled_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "batch transfer cancelled by the local application",
    )
}

/// Returns whether an I/O error is the cancellation sentinel.
#[must_use]
pub fn is_cancelled_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Interrupted
        && error.to_string() == "batch transfer cancelled by the local application"
}
