//! The platform BLE seam, crossed in both directions.
//!
//! The discovery crate never talks to a Bluetooth stack directly; it asks a
//! [`core::BleBackend`] to attach, and the backend pushes sightings into a
//! [`core::BleObservationSink`]. Both halves of that seam cross the FFI:
//!
//! ```text
//!   Kotlin/C# backend  ──start(request, sink)──▶  Rust coordinator
//!   Kotlin/C# backend  ──sink.observe(id, port)─▶ Rust coordinator
//! ```
//!
//! * [`BleBackend`] is a *foreign trait* (`#[uniffi::export(foreign)]`): the
//!   Kotlin/C# side implements it, Rust calls it. Every method returns
//!   `Result` so an exception raised in foreign code becomes an ordinary
//!   `Err` instead of panicking across the boundary.
//! * [`BleObservationSink`] is a Rust object handed *to* foreign code. It is
//!   an `Arc`-backed handle; dropping the last foreign reference releases the
//!   Rust side, and the coordinator never retains the foreign backend beyond
//!   a start/stop pair.
//!
//! # Threading
//!
//! The coordinator supervises the backend from Tokio worker threads, so
//! `start`, `stop` and `is_running` are invoked on those workers, and
//! `observe` may be called from whatever thread the platform radio callback
//! fires on. Three rules follow:
//!
//! 1. the foreign implementation must be thread-safe (UniFFI requires
//!    `Send + Sync` for foreign traits, and the bindings document it);
//! 2. `observe` must be cheap and non-blocking — the coordinator forwards it
//!    into an unbounded channel, and a slow sink stalls the radio thread;
//! 3. on Android, spawning Rust worker threads that call back into Kotlin
//!    makes JNA attach the thread to the JVM on every call unless the app
//!    attaches it once (see the crate README, "Threading and JNA").

use std::sync::Arc;

use libcore_discovery as core;

use crate::error::FfiError;

/// What the coordinator asks a backend to do.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct BleRequest {
    /// The 16-bit service UUID to advertise and scan on (`0xFE2C`).
    pub service_uuid: u16,
    /// This node's beacon id, as 32 hex digits.
    pub peer_id_hex: String,
    /// The port this node serves file transfers on.
    pub port: u16,
    /// Whether to scan for other advertisers as well as advertise.
    pub scanning: bool,
}

/// Receiver for sightings pushed by a foreign BLE backend.
///
/// This is a Rust object: foreign code holds a handle and calls
/// [`Self::observe`] from its radio callback. Calls are forwarded into the
/// coordinator's observation channel and never block on network I/O.
#[derive(uniffi::Object)]
pub struct BleObservationSink {
    inner: Arc<dyn core::BleObservationSink>,
}

#[uniffi::export]
impl BleObservationSink {
    /// Reports one sighting of a peer.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] when `peer_id_hex` is not
    /// exactly 32 hex digits. The coordinator drops observations for a stopped
    /// tier silently, so `Ok(())` does not imply the peer was recorded.
    pub fn observe(&self, peer_id_hex: String, port: u16) -> Result<(), FfiError> {
        let peer_id = core::PeerId::from_hex(&peer_id_hex)
            .map_err(|error| FfiError::invalid_argument(error.to_string()))?;
        self.inner.observe(core::BleObservation { peer_id, port });
        Ok(())
    }
}

/// A platform BLE backend, implemented in the foreign language.
///
/// `start` must be idempotent-safe: the coordinator always calls `stop`
/// before restarting a backend after a failure, but a backend asked to start
/// while already running may either return the existing attachment or replace
/// it — as long as exactly one sink receives observations. Observations are
/// pushed through the `sink` handle, which stays valid until `stop` returns
/// (and may be called from any thread until then).
#[uniffi::export(with_foreign)]
pub trait BleBackend: Send + Sync {
    /// Starts advertising (and optionally scanning), delivering sightings to
    /// `sink`.
    ///
    /// # Errors
    ///
    /// Return an error to tell the coordinator to retry with backoff; it
    /// surfaces as [`super::DiscoveryEvent::TierStateChanged`] with a
    /// `Degraded` or `Failed` state.
    fn start(&self, request: BleRequest, sink: Arc<BleObservationSink>) -> Result<(), FfiError>;

    /// Stops advertising and scanning and releases the radio.
    ///
    /// Must be safe to call when not started.
    ///
    /// # Errors
    ///
    /// Errors are logged as tier errors and otherwise ignored: there is no
    /// recovery action the coordinator could take.
    fn stop(&self) -> Result<(), FfiError>;

    /// Returns whether the radio is still attached.
    ///
    /// The coordinator polls this to notice a stack that dropped the radio
    /// behind its back (Bluetooth switched off, adapter removed) and restarts
    /// the backend.
    ///
    /// # Errors
    ///
    /// An error is treated as "not running" and triggers a restart.
    fn is_running(&self) -> Result<bool, FfiError>;
}

/// Adapts a foreign [`BleBackend`] to the discovery crate's trait.
pub(crate) struct PlatformBleBackend {
    callback: Arc<dyn BleBackend>,
}

impl PlatformBleBackend {
    /// Wraps a foreign backend implementation.
    pub(crate) fn new(callback: Arc<dyn BleBackend>) -> Self {
        Self { callback }
    }
}

impl core::BleBackend for PlatformBleBackend {
    fn start(
        &self,
        request: core::BleRequest,
        sink: Arc<dyn core::BleObservationSink>,
    ) -> Result<(), core::DiscoveryError> {
        let request = BleRequest {
            service_uuid: request.service_uuid,
            peer_id_hex: request.beacon.peer_id.to_hex(),
            port: request.beacon.port,
            scanning: request.scanning,
        };
        let sink = Arc::new(BleObservationSink { inner: sink });
        self.callback.start(request, sink).map_err(|error| {
            // `DiscoveryError::Backend` carries a `&'static str`; the I/O
            // variant is the one that can preserve the foreign error message,
            // so a Kotlin exception's text survives into the tier event.
            core::DiscoveryError::Io(std::io::Error::other(format!(
                "the platform BLE backend failed to start: {error}"
            )))
        })
    }

    fn stop(&self) {
        // Best effort: there is no recovery action for a failed detach, and
        // the coordinator is already tearing the tier down.
        let _ = self.callback.stop();
    }

    fn is_running(&self) -> bool {
        // An error is indistinguishable from a detached radio for our
        // purposes: the coordinator restarts the backend either way.
        self.callback.is_running().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn hex_decoding_is_validated_at_the_boundary() {
        // The sink without a coordinator is dead but still callable; the
        // string validation must happen before the observation is forwarded.
        struct NullSink;
        impl core::BleObservationSink for NullSink {
            fn observe(&self, _observation: core::BleObservation) {}
        }

        let sink = BleObservationSink {
            inner: Arc::new(NullSink),
        };
        assert!(sink
            .observe("00112233445566778899aabbccddeeff".to_owned(), 1)
            .is_ok());
        assert!(sink.observe("not-hex".to_owned(), 1).is_err());
        assert!(sink.observe(String::new(), 1).is_err());
    }
}
