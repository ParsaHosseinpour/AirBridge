//! The single Tokio runtime owned by the FFI boundary.
//!
//! Foreign executors must never be asked to provide a Tokio runtime. Kotlin
//! coroutines and .NET tasks poll the futures this crate returns; the actual
//! work — sockets, timers, filesystem — runs on one process-wide
//! multi-threaded runtime created here.
//!
//! That model has three consequences worth stating explicitly:
//!
//! * Every exported `async fn` is a thin shell: it clones the state it needs,
//!   calls [`run_on_runtime`], and returns a future that only awaits a
//!   [`tokio::task::JoinHandle`]. Polling that handle is executor-agnostic, so
//!   it works from a coroutine dispatcher, a .NET thread pool, or any other
//!   foreign scheduler.
//! * Dropping the returned future (a cancelled Kotlin `await`, a disposed
//!   .NET task) detaches the spawned task rather than stopping it. Cancellation
//!   is therefore explicit: see `FileTransferSession::cancel` and the
//!   [`cancel`](crate::cancel) module.
//! * Runtime creation failures are reported as ordinary FFI errors instead of
//!   panicking across the boundary.

use core::future::Future;
use std::{sync::OnceLock, time::Duration};

use tokio::runtime::{Builder, Runtime};

use crate::error::FfiError;

/// The process-wide runtime, or `None` if the OS refused to start it.
static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();

/// Returns the shared runtime, creating it on first use.
fn runtime() -> Option<&'static Runtime> {
    RUNTIME
        .get_or_init(|| {
            Builder::new_multi_thread()
                .enable_all()
                .thread_name("airbridge-ffi")
                // The transport engine is I/O-bound; a handful of workers is
                // plenty and keeps the thread footprint of a mobile process
                // predictable.
                .worker_threads(2)
                .thread_keep_alive(Duration::from_secs(30))
                .build()
                .ok()
        })
        .as_ref()
}

/// Enters the runtime on the calling thread.
///
/// Needed by synchronous exports that call APIs requiring an ambient Tokio
/// context — notably `DiscoveryCoordinator::start`, which spawns tasks.
/// Returns an error instead of panicking when the runtime is unavailable.
pub(crate) fn enter() -> Result<tokio::runtime::EnterGuard<'static>, FfiError> {
    runtime()
        .map(Runtime::enter)
        .ok_or_else(|| FfiError::internal("the shared tokio runtime could not be started"))
}

/// Spawns `future` onto the shared runtime and awaits its result.
///
/// A panic in the spawned task (or its cancellation) becomes
/// [`FfiError::Internal`]; foreign callers see a normal thrown error either
/// way. Note that the release profile is `panic = "abort"`, so in practice a
/// panic terminates the process — this mapping exists for the unwind-enabled
/// debug and test builds.
pub(crate) async fn run_on_runtime<T>(
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T, FfiError>
where
    T: Send + 'static,
{
    let runtime = runtime()
        .ok_or_else(|| FfiError::internal("the shared tokio runtime could not be started"))?;
    runtime
        .spawn(future)
        .await
        .map_err(|_| FfiError::internal("an internal task panicked or was cancelled"))
}

/// Spawns a background task without awaiting it.
///
/// Used for fire-and-forget shutdowns. Returns `false` when the runtime is
/// unavailable (the task is then simply not started); callers that need to
/// report the failure should check the return value.
pub(crate) fn spawn_detached<F>(future: F) -> bool
where
    F: Future<Output = ()> + Send + 'static,
{
    runtime().is_some_and(|runtime| {
        runtime.spawn(future);
        true
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn runtime_is_created_once_and_enters_cleanly() {
        assert!(runtime().is_some());
        let guard = enter().expect("runtime should be available");
        drop(guard);
        // Entering twice is fine; the runtime is shared.
        let _second = enter().expect("runtime should be available");
    }

    #[test]
    fn run_on_runtime_executes_the_future() {
        let outcome = runtime()
            .unwrap()
            .block_on(run_on_runtime(async { 41 + 1 }))
            .unwrap();
        assert_eq!(outcome, 42);
    }
}
