//! Cooperative cancellation that a foreign thread can trigger at any moment.
//!
//! UniFFI has no built-in cancellation: dropping the foreign future only
//! detaches the Rust task. The engines here therefore expose an explicit
//! `cancel()` (the pattern the UniFFI manual recommends), and this module is
//! the primitive behind it.
//!
//! # Why not just an `AtomicBool`?
//!
//! A transfer can be blocked inside `poll_read` on a socket that will never
//! produce another byte. Checking a flag only when the transfer makes progress
//! would leave `cancel()` inert exactly when it is needed. The canceller
//! therefore stores the waker of the task currently polling the socket and
//! wakes it, so the next poll observes the flag and terminates the transfer.
//!
//! # The handshake
//!
//! The flag and the waker are two separate atoms, so a race exists: `cancel()`
//! may store the flag before the poller stored its waker, or vice versa. Both
//! sides use `SeqCst` and the poller re-reads the flag *after* publishing its
//! waker. In a total `SeqCst` order at least one side must observe the other's
//! write:
//!
//! * poller reads `cancelled == false` afterwards, then `cancel()` stored the
//!   flag before that read, so it must read the waker published earlier and
//!   wake it;
//! * or the poller reads `cancelled == true` and reports cancellation itself.
//!
//! There is no window in which both sides miss each other.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

/// Shared cancellation flag with task wakeup.
#[derive(Debug, Default)]
pub(crate) struct Canceller {
    cancelled: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Canceller {
    /// Creates a canceller that has not been triggered.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation and wakes whatever task is waiting on it.
    ///
    /// Safe to call from any thread, any number of times. Never blocks and
    /// never allocates beyond the waker hand-off.
    pub(crate) fn request(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // The mutex cannot be poisoned in practice: every critical section in
        // this module is a single assignment. Recover anyway, matching the
        // coordinator's policy of preferring progress over panicking.
        let waker = self
            .waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Returns whether cancellation has been requested.
    pub(crate) fn is_requested(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Publishes `waker` and reports whether cancellation is already requested.
    ///
    /// `true` means the caller should stop immediately; `false` means the
    /// caller will be woken if `request` is called later. See the module
    /// documentation for the ordering argument.
    fn register(&self, waker: &Waker) -> bool {
        *self
            .waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(waker.clone());
        self.is_requested()
    }

    /// Registers the current task and reports whether cancellation is already
    /// requested. Used by I/O wrappers on every poll.
    pub(crate) fn poll_cancelled(&self, cx: &Context<'_>) -> bool {
        self.register(cx.waker())
    }
}

/// Future that completes when the canceller is triggered.
pub(crate) struct WaitCancelled<'a>(pub(crate) &'a Canceller);

impl Future for WaitCancelled<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0.register(cx.waker()) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn request_is_idempotent_and_observable() {
        let canceller = Canceller::new();
        assert!(!canceller.is_requested());
        canceller.request();
        canceller.request();
        assert!(canceller.is_requested());
    }

    #[test]
    fn waiting_future_completes_after_request() {
        let canceller = Arc::new(Canceller::new());
        let waker_flag = Arc::new(AtomicBool::new(false));

        // A minimal executor: poll once (pending), then request cancellation
        // from "another thread" and poll again. The waker records the wake.
        let mut wait = Box::pin(WaitCancelled(&canceller));
        let waker = Waker::from(Arc::new(FlagWaker(Arc::clone(&waker_flag))));
        let mut context = Context::from_waker(&waker);

        assert!(wait.as_mut().poll(&mut context).is_pending());
        canceller.request();
        assert!(
            waker_flag.load(Ordering::SeqCst),
            "request must wake the waiter"
        );
        assert!(wait.as_mut().poll(&mut context).is_ready());
    }

    #[test]
    fn waiting_after_request_is_immediately_ready() {
        let canceller = Canceller::new();
        canceller.request();
        let mut wait = Box::pin(WaitCancelled(&canceller));
        let waker = Waker::from(Arc::new(FlagWaker(Arc::new(AtomicBool::new(false)))));
        let mut context = Context::from_waker(&waker);
        assert!(wait.as_mut().poll(&mut context).is_ready());
    }

    /// A waker that only records that it was woken.
    struct FlagWaker(Arc<AtomicBool>);

    impl std::task::Wake for FlagWaker {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn concurrent_request_and_register_never_miss() {
        // Exercise the handshake: the poller publishes its waker in a tight
        // loop while another thread requests cancellation. Every iteration
        // must either observe cancellation or be woken.
        for _ in 0..1000 {
            let canceller = Arc::new(Canceller::new());
            let waiter = Arc::clone(&canceller);
            let woken = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&woken);

            let poller = std::thread::spawn(move || {
                let waker = Waker::from(Arc::new(FlagWaker(flag)));
                let mut context = Context::from_waker(&waker);
                let mut wait = Box::pin(WaitCancelled(&waiter));
                // One poll: either ready (observed) or pending (waker stored).
                match wait.as_mut().poll(&mut context) {
                    Poll::Ready(()) => true,
                    Poll::Pending => false,
                }
            });
            let trigger = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_micros(50));
                canceller.request();
            });

            let observed = poller.join().unwrap();
            trigger.join().unwrap();
            assert!(
                observed || woken.load(Ordering::SeqCst),
                "cancellation must be either observed or delivered as a wake"
            );
        }
    }
}
