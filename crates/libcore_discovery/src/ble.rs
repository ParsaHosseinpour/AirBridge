//! Tier 1 — BLE background advertising and scanning.
//!
//! AirBridge never *transfers* over BLE: the radio is far too slow for files.
//! The tier-1 job is to make presence known in the milliseconds after two
//! devices come into range, and to hand the resulting [`PeerId`]s to the
//! coordinator, which then merges them with whatever the IP tiers find.
//!
//! # Platform boundary
//!
//! The Bluetooth stack is platform code: CoreBluetooth on Apple platforms,
//! the Android BluetoothLeAdvertiser, WinRT on Windows. None of that belongs
//! in a portable crate, so this module defines a narrow seam instead:
//! [`BleBackend`]. A platform binding implements three operations — attach,
//! detach, and push an observation into a sink — and the coordinator owns
//! everything else: retry policy, event emission, peer deduplication.
//!
//! # Beacon format
//!
//! The beacon is the manufacturer data of a legacy advertisement and fits the
//! 31-byte legacy payload with room to spare:
//!
//! ```text
//! version:u8 | peer_id:16 | port:u16
//! ```
//!
//! Nineteen bytes, of which only the version byte and sixteen id bytes are
//! identity; the port is what a peer should connect to for file transfer. A
//! non-AirBridge advertiser on the same 16-bit UUID is rejected by the version
//! byte, not misparsed.
//!
//! Note what is *absent*: an IP address. A BLE-only sighting therefore puts a
//! peer in the table with no address at all. That is deliberate — the peer is
//! real and worth showing, and its address arrives the moment mDNS or the
//! subnet scan matches the same id, which is what the id is for.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::BLE_SERVICE_UUID;
use crate::error::DiscoveryError;
use crate::peer::{PeerId, PEER_ID_LEN};

/// Version byte of the AirBridge BLE beacon.
pub const BEACON_VERSION: u8 = 1;

/// Encoded beacon length: version + peer id + port.
pub const BEACON_LEN: usize = 1 + PEER_ID_LEN + 2;

/// A sighting reported by the platform BLE backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BleObservation {
    /// The observed peer.
    pub peer_id: PeerId,

    /// The port the peer wants file transfers on.
    pub port: u16,
}

/// The beacon this node broadcasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerBeacon {
    /// The broadcasting peer.
    pub peer_id: PeerId,

    /// The port the broadcasting peer serves transfers on.
    pub port: u16,
}

impl PeerBeacon {
    /// Creates a beacon for `peer_id` serving `port`.
    #[must_use]
    pub const fn new(peer_id: PeerId, port: u16) -> Self {
        Self { peer_id, port }
    }

    /// Encodes the beacon as manufacturer data.
    #[must_use]
    pub fn encode(&self) -> [u8; BEACON_LEN] {
        let mut out = [0_u8; BEACON_LEN];
        out[0] = BEACON_VERSION;
        out[1..1 + PEER_ID_LEN].copy_from_slice(self.peer_id.as_bytes());
        out[1 + PEER_ID_LEN..].copy_from_slice(&self.port.to_be_bytes());
        out
    }

    /// Decodes manufacturer data observed on the AirBridge service UUID.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoveryError::Encoding`] when the payload is not exactly
    /// [`BEACON_LEN`] bytes or carries a version this build does not know.
    pub fn decode(bytes: &[u8]) -> Result<Self, DiscoveryError> {
        if bytes.len() != BEACON_LEN {
            return Err(DiscoveryError::Encoding(
                "ble beacon must be exactly 19 bytes",
            ));
        }
        if bytes[0] != BEACON_VERSION {
            return Err(DiscoveryError::Encoding("unsupported ble beacon version"));
        }
        let mut peer_id = [0_u8; PEER_ID_LEN];
        peer_id.copy_from_slice(&bytes[1..1 + PEER_ID_LEN]);
        let port = u16::from_be_bytes([bytes[1 + PEER_ID_LEN], bytes[1 + PEER_ID_LEN + 1]]);
        Ok(Self {
            peer_id: PeerId::from_bytes(peer_id),
            port,
        })
    }
}

/// What the coordinator asks a BLE backend to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BleRequest {
    /// The 16-bit service UUID to advertise and scan on.
    pub service_uuid: u16,

    /// The beacon to advertise.
    pub beacon: PeerBeacon,

    /// Whether to scan for other advertisers as well as advertise. Scanning
    /// is what discovers peers; a backend may be started advertise-only to
    /// conserve radio time.
    pub scanning: bool,
}

impl BleRequest {
    /// Creates a request advertising `beacon` on the AirBridge UUID with
    /// scanning enabled.
    #[must_use]
    pub const fn new(beacon: PeerBeacon) -> Self {
        Self {
            service_uuid: BLE_SERVICE_UUID,
            beacon,
            scanning: true,
        }
    }
}

/// Receiver for sightings pushed by a BLE backend.
///
/// Implementations must be cheap and non-blocking: the platform stack calls
/// `observe` from whatever thread its callback fires on, and a slow sink stalls
/// the radio. The coordinator's own sink forwards into an unbounded channel
/// for exactly this reason.
pub trait BleObservationSink: Send + Sync + 'static {
    /// Reports one sighting.
    fn observe(&self, observation: BleObservation);
}

/// A platform BLE backend.
///
/// The trait is deliberately synchronous and object-safe: every platform API
/// behind it is a callback registration with a return code, and wrapping that
/// in a future would only add a state machine with no decision in it. The
/// coordinator supervises the lifecycle; backends just attach, detach and
/// report.
///
/// `start` must be idempotent-safe: the coordinator always calls `stop` before
/// restarting a backend after a failure, but a backend that is asked to start
/// while already running may either return the existing attachment or replace
/// it, as long as exactly one sink receives observations.
pub trait BleBackend: Send + Sync + 'static {
    /// Starts advertising (and optionally scanning), delivering sightings to
    /// `sink`.
    ///
    /// # Errors
    ///
    /// Returns a [`DiscoveryError::Backend`] describing why the radio could
    /// not be attached; the coordinator will retry with backoff.
    fn start(
        &self,
        request: BleRequest,
        sink: Arc<dyn BleObservationSink>,
    ) -> Result<(), DiscoveryError>;

    /// Stops advertising and scanning and releases the radio.
    ///
    /// Must be safe to call when not started.
    fn stop(&self);

    /// Returns whether the radio is still attached.
    ///
    /// The coordinator polls this to detect a platform stack that dropped the
    /// radio behind our back (Bluetooth turned off, adapter removed) and
    /// restarts the backend. Backends that cannot observe a detach keep the
    /// default.
    fn is_running(&self) -> bool {
        true
    }
}

/// Scriptable [`BleBackend`] for tests and for platforms without a radio.
///
/// The mock records the last request, counts lifecycle calls, can be armed to
/// fail the next start (to exercise the coordinator's backoff path) and can
/// deliver observations on demand, standing in for a nearby device.
#[derive(Clone, Default)]
pub struct MockBleBackend {
    inner: Arc<MockBleInner>,
}

#[derive(Default)]
struct MockBleInner {
    running: Mutex<Option<Running>>,
    starts: AtomicUsize,
    stops: AtomicUsize,
    fail_next_start: AtomicBool,
    emit_without_sink: AtomicBool,
}

struct Running {
    request: BleRequest,
    sink: Arc<dyn BleObservationSink>,
}

impl MockBleBackend {
    /// Creates a stopped mock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms the mock so the next `start` fails once, then succeeds.
    pub fn fail_next_start(&self) {
        self.inner.fail_next_start.store(true, Ordering::SeqCst);
    }

    /// Delivers an observation to the sink attached by the last successful
    /// start.
    ///
    /// Returns `false` when the mock is not running.
    pub fn emit(&self, observation: BleObservation) -> bool {
        let guard = lock(&self.inner.running);
        match guard.as_ref() {
            Some(running) => {
                running.sink.observe(observation);
                true
            }
            None => false,
        }
    }

    /// Returns the request of the last successful start.
    #[must_use]
    pub fn last_request(&self) -> Option<BleRequest> {
        lock(&self.inner.running)
            .as_ref()
            .map(|running| running.request)
    }

    /// Returns how many times `start` was called, successful or not.
    #[must_use]
    pub fn start_count(&self) -> usize {
        self.inner.starts.load(Ordering::SeqCst)
    }

    /// Returns how many times `stop` was called.
    #[must_use]
    pub fn stop_count(&self) -> usize {
        self.inner.stops.load(Ordering::SeqCst)
    }

    /// Returns whether the mock currently has a sink attached.
    #[must_use]
    pub fn is_running(&self) -> bool {
        lock(&self.inner.running).is_some()
    }

    /// Returns whether an observation was ever emitted with no sink attached;
    /// useful to assert that the coordinator cleaned up the radio.
    #[must_use]
    pub fn emitted_without_sink(&self) -> bool {
        self.inner.emit_without_sink.load(Ordering::SeqCst)
    }
}

/// Locks a mutex, recovering the guard if a previous holder panicked.
///
/// The invariant behind these mutexes is one `Option` field, so a poisoned
/// lock still contains a consistent value; refusing to serve the coordinator
/// after an unrelated panic would be strictly worse.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl BleBackend for MockBleBackend {
    fn start(
        &self,
        request: BleRequest,
        sink: Arc<dyn BleObservationSink>,
    ) -> Result<(), DiscoveryError> {
        self.inner.starts.fetch_add(1, Ordering::SeqCst);
        if self.inner.fail_next_start.swap(false, Ordering::SeqCst) {
            return Err(DiscoveryError::Backend("mock start failure"));
        }
        *lock(&self.inner.running) = Some(Running { request, sink });
        Ok(())
    }

    fn stop(&self) {
        self.inner.stops.fetch_add(1, Ordering::SeqCst);
        let previous = lock(&self.inner.running).take();
        if previous.is_none() {
            self.inner.emit_without_sink.store(true, Ordering::SeqCst);
        }
    }

    fn is_running(&self) -> bool {
        lock(&self.inner.running).is_some()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn beacon_roundtrips() {
        let id = PeerId::from_bytes([0xab; PEER_ID_LEN]);
        let beacon = PeerBeacon::new(id, 42517);
        let encoded = beacon.encode();
        assert_eq!(encoded.len(), BEACON_LEN);
        assert_eq!(encoded[0], BEACON_VERSION);
        assert_eq!(PeerBeacon::decode(&encoded).unwrap(), beacon);
    }

    #[test]
    fn beacon_rejects_foreign_advertisers() {
        let id = PeerId::from_bytes([1; PEER_ID_LEN]);
        let mut encoded = PeerBeacon::new(id, 1).encode();
        encoded[0] = BEACON_VERSION + 1;
        assert!(PeerBeacon::decode(&encoded).is_err());

        let short = encoded[..BEACON_LEN - 1].to_vec();
        assert!(PeerBeacon::decode(&short).is_err());
        assert!(PeerBeacon::decode(&[]).is_err());
    }

    #[test]
    fn request_defaults_to_airbridge_uuid() {
        let request = BleRequest::new(PeerBeacon::new(PeerId::from_bytes([2; PEER_ID_LEN]), 9));
        assert_eq!(request.service_uuid, BLE_SERVICE_UUID);
        assert!(request.scanning);
    }

    #[test]
    fn mock_records_lifecycle() {
        let mock = MockBleBackend::new();
        assert!(!mock.is_running());
        assert!(!mock.emit(BleObservation {
            peer_id: PeerId::from_bytes([3; PEER_ID_LEN]),
            port: 1,
        }));

        let request = BleRequest::new(PeerBeacon::new(PeerId::from_bytes([4; PEER_ID_LEN]), 7));
        let seen = Arc::new(Mutex::new(Vec::new()));
        struct Collector(Arc<Mutex<Vec<BleObservation>>>);
        impl BleObservationSink for Collector {
            fn observe(&self, observation: BleObservation) {
                lock(&self.0).push(observation);
            }
        }
        mock.start(request, Arc::new(Collector(Arc::clone(&seen))))
            .unwrap();
        assert_eq!(mock.last_request(), Some(request));
        assert_eq!(mock.start_count(), 1);

        let observation = BleObservation {
            peer_id: PeerId::from_bytes([5; PEER_ID_LEN]),
            port: 42517,
        };
        assert!(mock.emit(observation));
        assert_eq!(*lock(&seen), vec![observation]);

        mock.stop();
        assert!(!mock.is_running());
        assert_eq!(mock.stop_count(), 1);
        assert_eq!(mock.last_request(), None);
    }

    #[test]
    fn mock_fails_once_when_armed() {
        let mock = MockBleBackend::new();
        mock.fail_next_start();
        let request = BleRequest::new(PeerBeacon::new(PeerId::from_bytes([6; PEER_ID_LEN]), 2));
        let sink: Arc<dyn BleObservationSink> = Arc::new(NoopSink);
        assert!(mock.start(request, Arc::clone(&sink)).is_err());
        assert!(mock.last_request().is_none());
        assert!(mock.start(request, sink).is_ok());
        assert_eq!(mock.start_count(), 2);
    }

    struct NoopSink;

    impl BleObservationSink for NoopSink {
        fn observe(&self, _observation: BleObservation) {}
    }
}
