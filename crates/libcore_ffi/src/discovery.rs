//! Peer discovery exposed as an object with a pull-based event stream.
//!
//! # Mapping to foreign concurrency
//!
//! The Rust coordinator publishes `DiscoveryEvent`s on a lossy broadcast
//! channel. The FFI engine exposes [`PeerDiscoveryEngine::next_event`], an
//! `async fn` that the foreign side drains in a loop:
//!
//! * Kotlin wraps that loop in `flow { … }` (see `bindings/kotlin/Flow.kt`);
//! * C# wraps it in an async pump that raises a .NET event (see
//!   `bindings/csharp/SessionEvents.cs`).
//!
//! Pull instead of push is deliberate: a callback interface would let the
//! rust side call arbitrary foreign code from a Tokio worker at any moment,
//! which forces every listener implementation to be thread-safe *and* forces
//! JNA to attach the worker thread to the JVM on each callback. Polling keeps
//! all foreign execution on the foreign executor and gives the consumer
//! natural backpressure: the next event is only requested when the previous
//! one has been handled.
//!
//! # Loss is explicit, not silent
//!
//! The broadcast ring holds [`EVENT_CHANNEL_CAPACITY`] events. A consumer that
//! falls further behind receives [`DiscoveryEvent::Lagged`] with the number of
//! dropped events instead of an exception. The peer table — reachable through
//! [`PeerDiscoveryEngine::peers`] — is authoritative, so a lagging UI can
//! simply re-snapshot.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use libcore_discovery as core;
use tokio::sync::{broadcast, Mutex as AsyncMutex};

use crate::ble::{BleBackend, PlatformBleBackend};
use crate::error::FfiError;
use crate::runtime;

/// Controls whether and when the active `/24` subnet probe runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SubnetPolicy {
    /// Never probe. For hosts whose network administrator would notice a
    /// `/24` sweep.
    Disabled,
    /// Probe only when mDNS failed or found nobody within the grace period.
    /// This is the discovery crate's default.
    Fallback,
    /// Probe immediately at startup, in parallel with the passive tiers.
    Always,
}

impl From<SubnetPolicy> for core::SubnetPolicy {
    fn from(policy: SubnetPolicy) -> Self {
        match policy {
            SubnetPolicy::Disabled => Self::Disabled,
            SubnetPolicy::Fallback => Self::Fallback,
            SubnetPolicy::Always => Self::Always,
        }
    }
}

/// Discovery configuration, flattened for foreign construction.
///
/// Every field has a foreign-visible default, so `DiscoveryConfig()` is a
/// valid configuration in Kotlin and C#. Durations are milliseconds because
/// that is what both target runtimes use natively; the Rust side converts.
#[derive(Clone, Debug, uniffi::Record)]
pub struct DiscoveryConfig {
    /// This node's identifier as 32 hex digits, or `None` to generate one.
    ///
    /// Persist the generated value (read it back from
    /// [`PeerDiscoveryEngine::peer_id_hex`]) if the app wants a stable
    /// identity across restarts; the discovery crate treats ids as opaque,
    /// not as a trust anchor.
    #[uniffi(default)]
    pub peer_id_hex: Option<String>,

    /// Human-readable name advertised to other peers.
    #[uniffi(default)]
    pub peer_name: Option<String>,

    /// Port other peers should connect to for file transfer.
    #[uniffi(default = 42517)]
    pub listen_port: u16,

    /// Whether the BLE tier is started. Ignored without a backend.
    #[uniffi(default = true)]
    pub enable_ble: bool,

    /// Whether the mDNS/DNS-SD tier is started.
    #[uniffi(default = true)]
    pub enable_mdns: bool,

    /// Subnet policy; `None` means [`SubnetPolicy::Fallback`].
    #[uniffi(default)]
    pub subnet_policy: Option<SubnetPolicy>,

    /// How long mDNS may stay silent before a fallback sweep starts.
    #[uniffi(default = 15000)]
    pub mdns_grace_ms: u64,

    /// How long a peer survives without a new observation.
    #[uniffi(default = 60000)]
    pub peer_ttl_ms: u64,
}

impl DiscoveryConfig {
    /// Builds the discovery crate's configuration, generating a peer id when
    /// none was supplied.
    fn to_core(&self) -> Result<(core::DiscoveryConfig, String), FfiError> {
        let peer_id = match &self.peer_id_hex {
            Some(hex) => core::PeerId::from_hex(hex)
                .map_err(|error| FfiError::invalid_argument(error.to_string()))?,
            None => {
                core::PeerId::generate().map_err(|error| FfiError::internal(error.to_string()))?
            }
        };
        let peer_id_hex = peer_id.to_hex();

        let mut config = core::DiscoveryConfig::new(peer_id, self.listen_port)
            .with_ble(self.enable_ble)
            .with_mdns_enabled(self.enable_mdns)
            .with_subnet_policy(self.subnet_policy.unwrap_or(SubnetPolicy::Fallback).into())
            .with_mdns_grace(checked_duration(self.mdns_grace_ms, "mdns_grace_ms")?)
            .with_peer_ttl(checked_duration(self.peer_ttl_ms, "peer_ttl_ms")?);
        if let Some(name) = &self.peer_name {
            config = config.with_name(name.clone());
        }
        Ok((config, peer_id_hex))
    }
}

/// Converts a millisecond count, rejecting values that cannot be a duration.
fn checked_duration(millis: u64, field: &str) -> Result<Duration, FfiError> {
    if millis == 0 {
        return Err(FfiError::invalid_argument(format!(
            "{field} must be non-zero"
        )));
    }
    Ok(Duration::from_millis(millis))
}

/// One of the three discovery tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum DiscoveryTier {
    /// Tier 1: BLE advertisement and scan.
    Ble,
    /// Tier 2: mDNS/DNS-SD.
    Mdns,
    /// Tier 3: active IPv4 subnet probe.
    Subnet,
}

impl From<core::TierId> for DiscoveryTier {
    fn from(tier: core::TierId) -> Self {
        match tier {
            core::TierId::Ble => Self::Ble,
            core::TierId::Mdns => Self::Mdns,
            core::TierId::Subnet => Self::Subnet,
        }
    }
}

/// Lifecycle state of a single tier.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TierState {
    /// Coming up: binding sockets, attaching the BLE backend.
    Starting,
    /// Running and expected to produce peers.
    Active,
    /// Running but diminished; the string explains why.
    Degraded {
        /// Human-readable reason.
        reason: String,
    },
    /// The tier stopped or failed to start. Under [`SubnetPolicy::Fallback`]
    /// a failed mDNS tier is exactly what triggers the active sweep.
    Failed {
        /// Human-readable detail.
        message: String,
    },
    /// Shut down cleanly.
    Stopped,
}

impl From<core::TierState> for TierState {
    fn from(state: core::TierState) -> Self {
        match state {
            core::TierState::Starting => Self::Starting,
            core::TierState::Active => Self::Active,
            core::TierState::Degraded(reason) => Self::Degraded {
                reason: reason.to_owned(),
            },
            core::TierState::Failed(error) => Self::Failed {
                message: error.to_string(),
            },
            core::TierState::Stopped => Self::Stopped,
        }
    }
}

/// Everything the discovery layer knows about one peer.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PeerInfo {
    /// Stable identifier as 32 lowercase hex digits.
    ///
    /// The id is carried as a string rather than a byte array on purpose:
    /// strings are immutable and structurally comparable in both target
    /// languages, whereas Kotlin `ByteArray` breaks `data class` equality and
    /// C# `byte[]` compares by reference. The canonical 16 bytes are one
    /// `decodeHex` away when an app genuinely needs them.
    pub id_hex: String,

    /// Advertised display name. Not identity: a hostile peer can set any name.
    pub name: Option<String>,

    /// Every observed address, rendered as `ip:port` and sorted.
    pub addresses: Vec<String>,
}

impl From<&core::PeerInfo> for PeerInfo {
    fn from(peer: &core::PeerInfo) -> Self {
        Self {
            id_hex: peer.id.to_hex(),
            name: peer.name.clone(),
            addresses: peer.addresses.iter().map(ToString::to_string).collect(),
        }
    }
}

/// Something worth telling the application about.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum DiscoveryEvent {
    /// A peer entered the table.
    PeerFound {
        /// The new peer.
        peer: PeerInfo,
    },
    /// A known peer gained a name or an address.
    PeerUpdated {
        /// The merged peer.
        peer: PeerInfo,
    },
    /// A peer's last observation aged past the TTL and it was evicted.
    PeerLost {
        /// The evicted id, as 32 hex digits.
        id_hex: String,
    },
    /// A tier changed lifecycle state.
    TierStateChanged {
        /// The tier that changed.
        tier: DiscoveryTier,
        /// Its new state.
        state: TierState,
    },
    /// A tier reported a non-fatal error.
    TierError {
        /// The tier that reported it.
        tier: DiscoveryTier,
        /// Human-readable detail.
        message: String,
    },
    /// A subnet sweep started over `subnets` networks.
    SubnetScanStarted {
        /// Number of networks in the sweep.
        subnets: u32,
    },
    /// A subnet sweep completed.
    SubnetScanFinished {
        /// Host addresses probed.
        probed: u32,
        /// Hosts that answered the probe.
        responded: u32,
        /// Responses that produced a peer entry.
        discovered: u32,
    },
    /// The coordinator stopped all tiers; the stream ends after this event.
    Shutdown,
    /// The consumer fell behind and `missed` events were dropped. Re-read
    /// [`PeerDiscoveryEngine::peers`] to recover the authoritative state.
    Lagged {
        /// Number of dropped events.
        missed: u64,
    },
}

/// Maps a coordinator event, dropping variants this build does not know.
fn map_event(event: core::DiscoveryEvent) -> Option<DiscoveryEvent> {
    Some(match event {
        core::DiscoveryEvent::PeerFound(peer) => DiscoveryEvent::PeerFound {
            peer: PeerInfo::from(&peer),
        },
        core::DiscoveryEvent::PeerUpdated(peer) => DiscoveryEvent::PeerUpdated {
            peer: PeerInfo::from(&peer),
        },
        core::DiscoveryEvent::PeerLost(id) => DiscoveryEvent::PeerLost {
            id_hex: id.to_hex(),
        },
        core::DiscoveryEvent::TierStateChanged { tier, state } => {
            DiscoveryEvent::TierStateChanged {
                tier: tier.into(),
                state: state.into(),
            }
        }
        core::DiscoveryEvent::TierError { tier, error } => DiscoveryEvent::TierError {
            tier: tier.into(),
            message: error.to_string(),
        },
        core::DiscoveryEvent::SubnetScanStarted { subnets } => DiscoveryEvent::SubnetScanStarted {
            subnets: u32::try_from(subnets).unwrap_or(u32::MAX),
        },
        core::DiscoveryEvent::SubnetScanFinished {
            probed,
            responded,
            discovered,
        } => DiscoveryEvent::SubnetScanFinished {
            probed: u32::try_from(probed).unwrap_or(u32::MAX),
            responded: u32::try_from(responded).unwrap_or(u32::MAX),
            discovered: u32::try_from(discovered).unwrap_or(u32::MAX),
        },
        core::DiscoveryEvent::Shutdown => DiscoveryEvent::Shutdown,
        // `DiscoveryEvent` is `#[non_exhaustive]`; new variants must not break
        // existing bindings. They are skipped, and the peer snapshot remains
        // the source of truth.
        _ => return None,
    })
}

/// Mutable engine state, guarded for short synchronous critical sections.
struct EngineState {
    config: DiscoveryConfig,
    peer_id_hex: Option<String>,
    handle: Option<core::DiscoveryHandle>,
}

/// Starts, observes and stops AirBridge peer discovery.
///
/// The object is a facade over `DiscoveryCoordinator`. It owns a background
/// runtime, so `start` may be called from any foreign thread. Dropping the
/// last foreign reference signals shutdown (without waiting); call
/// [`PeerDiscoveryEngine::stop`] when deterministic teardown matters.
#[derive(uniffi::Object)]
pub struct PeerDiscoveryEngine {
    state: Mutex<EngineState>,
    events: Arc<AsyncMutex<Option<broadcast::Receiver<core::DiscoveryEvent>>>>,
}

#[uniffi::export]
impl PeerDiscoveryEngine {
    /// Creates an engine; nothing is started until [`Self::start`] is called.
    #[uniffi::constructor]
    #[must_use]
    pub fn new(config: DiscoveryConfig) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(EngineState {
                config,
                peer_id_hex: None,
                handle: None,
            }),
            events: Arc::new(AsyncMutex::new(None)),
        })
    }

    /// Starts discovery, optionally with a platform BLE backend.
    ///
    /// `ble_backend` is a foreign implementation of [`BleBackend`] — Kotlin's
    /// `BluetoothLeAdvertiser`, C#'s `BluetoothLEAdvertisementWatcher`, or a
    /// test double. Passing `None` is valid: the BLE tier reports itself as
    /// degraded and the IP tiers carry on.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::InvalidArgument`] for a malformed peer id or a zero
    /// duration, [`FfiError::AlreadyStarted`] if called twice, and
    /// [`FfiError::Discovery`] if the coordinator rejects the configuration.
    pub fn start(&self, ble_backend: Option<Arc<dyn BleBackend>>) -> Result<(), FfiError> {
        let mut state = lock(&self.state);
        if state.handle.is_some() {
            return Err(FfiError::AlreadyStarted);
        }

        let (core_config, peer_id_hex) = state.config.to_core()?;

        // The coordinator spawns tasks, so it must run with the shared
        // runtime's context active on this thread.
        let _guard = runtime::enter()?;

        let backend: Option<Arc<dyn core::BleBackend>> = ble_backend.map(|callback| {
            Arc::new(PlatformBleBackend::new(callback)) as Arc<dyn core::BleBackend>
        });

        let handle = core::DiscoveryCoordinator::start(core_config, backend)?;
        // Subscribe before returning: the broadcast channel does not replay
        // history, so any event published before this line is lost.
        let receiver = handle.events();

        // `try_lock` cannot fail here: no consumer can hold the event lock
        // before a handle exists, and `state` serialises concurrent starts.
        let mut events = self
            .events
            .try_lock()
            .map_err(|_| FfiError::internal("the event stream was unexpectedly locked"))?;
        *events = Some(receiver);

        state.peer_id_hex = Some(peer_id_hex);
        state.handle = Some(handle);
        Ok(())
    }

    /// Returns this node's peer id (generated at start if none was supplied).
    #[must_use]
    pub fn peer_id_hex(&self) -> Option<String> {
        lock(&self.state).peer_id_hex.clone()
    }

    /// Returns a snapshot of every live peer, ordered by id.
    ///
    /// This is the authoritative view; it is valid even when the event stream
    /// has lagged.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::NotStarted`] before [`Self::start`].
    pub fn peers(&self) -> Result<Vec<PeerInfo>, FfiError> {
        let state = lock(&self.state);
        let handle = state.handle.as_ref().ok_or(FfiError::NotStarted)?;
        Ok(handle.peers().iter().map(PeerInfo::from).collect())
    }

    /// Requests an immediate subnet sweep.
    ///
    /// Returns `false` when discovery is stopped, the policy is
    /// [`SubnetPolicy::Disabled`], or a sweep is already running (requests are
    /// coalesced).
    #[must_use]
    pub fn trigger_subnet_scan(&self) -> bool {
        lock(&self.state)
            .handle
            .as_ref()
            .is_some_and(core::DiscoveryHandle::trigger_subnet_scan)
    }

    /// Stops discovery and tears the tiers down in the background.
    ///
    /// Non-blocking and idempotent. The coordinator sends its goodbye
    /// announcements, publishes [`DiscoveryEvent::Shutdown`], and only then
    /// closes the event stream, so a UI that is draining `next_event` still
    /// observes the terminal event.
    pub fn stop(&self) {
        let handle = lock(&self.state).handle.take();
        if let Some(handle) = handle {
            runtime::spawn_detached(handle.shutdown());
        }
    }

    /// Returns whether discovery is currently running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        lock(&self.state).handle.is_some()
    }

    /// Waits for the next discovery event.
    ///
    /// Resolves to `None` once the coordinator has stopped and the stream is
    /// closed; a Kotlin `flow` or C# pump uses that as its termination signal.
    ///
    /// # Errors
    ///
    /// Returns [`FfiError::Internal`] if the shared runtime is unavailable.
    pub async fn next_event(&self) -> Result<Option<DiscoveryEvent>, FfiError> {
        let events = Arc::clone(&self.events);
        runtime::run_on_runtime(async move {
            let mut guard = events.lock().await;
            let Some(receiver) = guard.as_mut() else {
                return Ok(None);
            };
            loop {
                match receiver.recv().await {
                    Ok(event) => {
                        if let Some(event) = map_event(event) {
                            return Ok(Some(event));
                        }
                        // Unknown future variant: keep waiting.
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        return Ok(Some(DiscoveryEvent::Lagged { missed }));
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        *guard = None;
                        return Ok(None);
                    }
                }
            }
        })
        .await?
    }
}

/// Locks a std mutex, recovering from poisoning.
///
/// The engine state is a handful of `Option`s; a panic elsewhere cannot leave
/// it half-updated, so serving the state is better than failing.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
