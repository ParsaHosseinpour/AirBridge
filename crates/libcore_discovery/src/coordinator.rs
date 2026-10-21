//! The coordinator: supervises tiers, deduplicates peers and decides when to
//! escalate from passive discovery to an active subnet sweep.
//!
//! # Ownership
//!
//! [`DiscoveryCoordinator::start`] spawns one coordinator task and one task
//! per enabled tier. The coordinator owns the [`PeerTable`]; tiers only push
//! [`Observation`]s into a channel. That single-writer arrangement is what
//! makes deduplication and event ordering predictable: the same peer seen on
//! BLE, mDNS and the subnet scanner is inserted once, and `PeerFound` is
//! emitted once.
//!
//! # Escalation
//!
//! [`SubnetPolicy`] decides what tier 3 does:
//!
//! * `Disabled` — never probe.
//! * `Fallback` — probe when the mDNS tier reports a fatal failure, or when
//!   the grace period expires without a single mDNS sighting.
//! * `Always` — probe at startup, in parallel with the passive tiers.
//!
//! A fallback scan is one-shot. mDNS keeps browsing in the background, so a
//! peer that appears later is still found; [`DiscoveryHandle::trigger_subnet_scan`]
//! exists for applications that want an explicit refresh.
//!
//! # Shutdown
//!
//! Dropping the handle signals shutdown; [`DiscoveryHandle::shutdown`] does
//! the same and waits for the coordinator to finish. The coordinator asks each
//! tier to stop (BLE detaches its radio, mDNS sends a goodbye packet) before
//! publishing [`DiscoveryEvent::Shutdown`]. A subnet sweep is aborted rather
//! than awaited, so shutdown is never delayed by a probe timeout.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

use crate::ble::{BleBackend, BleObservation, BleObservationSink, BleRequest, PeerBeacon};
use crate::config::{DiscoveryConfig, SubnetPolicy};
use crate::error::DiscoveryError;
use crate::event::{
    publish, publish_error, publish_state, DiscoveryEvent, EventBus, TierId, TierState,
};
use crate::mdns;
use crate::peer::{PeerChange, PeerInfo, PeerTable};
use crate::subnet;

/// Capacity of the public event broadcast channel.
///
/// Slow subscribers lose the oldest events once the ring fills; the peer table
/// remains the authoritative snapshot, so losing an intermediate event is
/// never fatal.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// An observation pushed by a tier into the coordinator.
#[derive(Debug)]
pub(crate) enum Observation {
    /// A peer seen by the BLE tier. It may have no address at all.
    Ble(PeerInfo),

    /// A peer assembled from DNS-SD records.
    Mdns(PeerInfo),

    /// A peer that answered a subnet probe.
    Subnet(PeerInfo),

    /// The mDNS tier could not start. Only meaningful under
    /// [`SubnetPolicy::Fallback`], where it triggers the sweep immediately.
    MdnsUnavailable,
}

/// Starts discovery and returns a handle that owns it.
///
/// `backend` is the platform BLE implementation. Passing `None` is valid on
/// platforms without a radio binding: the tier is reported as degraded and
/// the IP tiers carry on.
pub struct DiscoveryCoordinator;

impl DiscoveryCoordinator {
    /// Validates `config`, spawns the tiers and returns their handle.
    ///
    /// Must be called from inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoveryError::InvalidConfig`] when `config` fails
    /// validation or when no runtime is available. Configuration problems are
    /// the only errors that prevent startup; tier failures surface as events.
    pub fn start(
        config: DiscoveryConfig,
        backend: Option<Arc<dyn BleBackend>>,
    ) -> Result<DiscoveryHandle, DiscoveryError> {
        config.validate()?;
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(DiscoveryError::InvalidConfig(
                "discovery must be started from inside a tokio runtime",
            ));
        }

        let (bus, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (observations_tx, observations_rx) = mpsc::unbounded_channel();
        let (trigger_tx, trigger_rx) = mpsc::unbounded_channel();
        let peers = Arc::new(Mutex::new(PeerTable::new()));
        let manual_scan = config.subnet_policy != SubnetPolicy::Disabled;

        let state = CoordinatorState {
            config,
            backend,
            bus: bus.clone(),
            peers: Arc::clone(&peers),
            observations_tx,
            observations: observations_rx,
            triggers: trigger_rx,
            shutdown: shutdown_rx,
        };
        let task = tokio::spawn(run(state));

        Ok(DiscoveryHandle {
            bus,
            peers,
            shutdown: shutdown_tx,
            trigger: trigger_tx,
            task: Some(task),
            manual_scan,
        })
    }
}

/// Handle to a running discovery coordinator.
///
/// The handle is the application's whole interface to discovery: subscribe to
/// events, read the peer snapshot, trigger a scan, or shut everything down.
/// Dropping it also shuts discovery down, but does not wait; call
/// [`Self::shutdown`] when deterministic teardown matters.
pub struct DiscoveryHandle {
    bus: EventBus,
    peers: Arc<Mutex<PeerTable>>,
    shutdown: watch::Sender<bool>,
    trigger: mpsc::UnboundedSender<()>,
    task: Option<JoinHandle<()>>,
    manual_scan: bool,
}

impl DiscoveryHandle {
    /// Subscribes to the event stream.
    ///
    /// Subscribe immediately after [`DiscoveryCoordinator::start`]: the
    /// channel is lossy and does not replay history. A late subscriber can
    /// always call [`Self::peers`] for the current state.
    #[must_use]
    pub fn events(&self) -> broadcast::Receiver<DiscoveryEvent> {
        self.bus.subscribe()
    }

    /// Returns a snapshot of every live peer, ordered by id.
    #[must_use]
    pub fn peers(&self) -> Vec<PeerInfo> {
        lock_table(&self.peers).snapshot()
    }

    /// Requests an immediate subnet sweep.
    ///
    /// Returns `false` when the policy is [`SubnetPolicy::Disabled`] or the
    /// coordinator has already stopped. A request while a sweep is already
    /// running is accepted and coalesced: no second sweep is started.
    pub fn trigger_subnet_scan(&self) -> bool {
        self.manual_scan && self.trigger.send(()).is_ok()
    }

    /// Stops discovery and waits for the coordinator to finish.
    ///
    /// Tier tasks are asked to clean up (BLE radio detach, mDNS goodbye) and
    /// the coordinator publishes [`DiscoveryEvent::Shutdown`].
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for DiscoveryHandle {
    fn drop(&mut self) {
        // The task observes the flag and unwinds its tiers. Waiting is the
        // caller's choice (`shutdown`), not a side effect of dropping.
        let _ = self.shutdown.send(true);
    }
}

/// Everything the coordinator task owns.
struct CoordinatorState {
    config: DiscoveryConfig,
    backend: Option<Arc<dyn BleBackend>>,
    bus: EventBus,
    peers: Arc<Mutex<PeerTable>>,
    observations_tx: mpsc::UnboundedSender<Observation>,
    observations: mpsc::UnboundedReceiver<Observation>,
    triggers: mpsc::UnboundedReceiver<()>,
    shutdown: watch::Receiver<bool>,
}

/// Runs until shutdown, then stops every tier.
async fn run(mut state: CoordinatorState) {
    let mut ble_task = None;
    if state.config.ble_enabled {
        if let Some(backend) = state.backend.take() {
            let request = BleRequest::new(PeerBeacon::new(
                state.config.peer_id,
                state.config.listen_port,
            ));
            ble_task = Some(tokio::spawn(ble_supervisor(
                backend,
                request,
                state.bus.clone(),
                state.observations_tx.clone(),
                state.config.ble_retry_backoff,
                state.shutdown.clone(),
            )));
        } else {
            publish_state(
                &state.bus,
                TierId::Ble,
                TierState::Degraded("no BLE backend is configured for this platform"),
            );
        }
    }

    let mdns_task = if state.config.mdns_enabled {
        Some(tokio::spawn(mdns::run(
            state.config.mdns.clone(),
            state.config.peer_id,
            state.config.peer_name.clone(),
            state.config.listen_port,
            state.bus.clone(),
            state.observations_tx.clone(),
            state.shutdown.clone(),
        )))
    } else {
        publish_state(&state.bus, TierId::Mdns, TierState::Stopped);
        None
    };

    let scan_running = Arc::new(AtomicBool::new(false));
    if state.config.subnet_policy == SubnetPolicy::Always {
        spawn_scan(
            &state.config,
            &state.bus,
            &state.observations_tx,
            &state.shutdown,
            &scan_running,
        );
    }

    let mut grace_active = state.config.subnet_policy == SubnetPolicy::Fallback;
    let grace_deadline = tokio::time::Instant::now() + state.config.mdns_grace;
    let mut mdns_found_peer = false;

    let mut sweep = tokio::time::interval(state.config.sweep_interval);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            changed = state.shutdown.changed() => {
                if changed.is_err() || *state.shutdown.borrow() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(grace_deadline), if grace_active => {
                grace_active = false;
                if !mdns_found_peer {
                    spawn_scan(
                        &state.config,
                        &state.bus,
                        &state.observations_tx,
                        &state.shutdown,
                        &scan_running,
                    );
                }
            }
            Some(observation) = state.observations.recv() => {
                match observation {
                    Observation::Mdns(peer) => {
                        mdns_found_peer = true;
                        grace_active = false;
                        record_peer(&state.peers, &state.bus, peer);
                    }
                    Observation::Ble(peer) | Observation::Subnet(peer) => {
                        record_peer(&state.peers, &state.bus, peer);
                    }
                    Observation::MdnsUnavailable => {
                        if state.config.subnet_policy == SubnetPolicy::Fallback {
                            grace_active = false;
                            spawn_scan(
                                &state.config,
                                &state.bus,
                                &state.observations_tx,
                                &state.shutdown,
                                &scan_running,
                            );
                        }
                    }
                }
            }
            trigger = state.triggers.recv() => {
                if trigger.is_none() {
                    // The handle is gone; its Drop already set the shutdown
                    // flag, so leaving is the right move even if the watch
                    // notification is not observed first.
                    break;
                }
                spawn_scan(
                    &state.config,
                    &state.bus,
                    &state.observations_tx,
                    &state.shutdown,
                    &scan_running,
                );
            }
            _ = sweep.tick() => {
                let evicted = lock_table(&state.peers)
                    .expire(Instant::now(), state.config.peer_ttl);
                for id in evicted {
                    publish(&state.bus, DiscoveryEvent::PeerLost(id));
                }
            }
        }
    }

    if let Some(task) = ble_task {
        let _ = task.await;
    }
    if let Some(task) = mdns_task {
        let _ = task.await;
    }
    publish(&state.bus, DiscoveryEvent::Shutdown);
}

/// Spawns a subnet sweep unless one is already running (or policy forbids it).
fn spawn_scan(
    config: &DiscoveryConfig,
    bus: &EventBus,
    observations: &mpsc::UnboundedSender<Observation>,
    shutdown: &watch::Receiver<bool>,
    running: &Arc<AtomicBool>,
) {
    if config.subnet_policy == SubnetPolicy::Disabled {
        return;
    }
    if running.swap(true, Ordering::SeqCst) {
        return;
    }
    let scan = config.subnet_scan.clone();
    let local_id = config.peer_id;
    let bus = bus.clone();
    let observations = observations.clone();
    let shutdown = shutdown.clone();
    let running = Arc::clone(running);
    tokio::spawn(async move {
        subnet::run_scan(scan, local_id, bus, observations, shutdown).await;
        running.store(false, Ordering::SeqCst);
    });
}

/// Inserts a peer and publishes the matching event.
fn record_peer(peers: &Arc<Mutex<PeerTable>>, bus: &EventBus, peer: PeerInfo) {
    let (change, merged) = lock_table(peers).insert(peer, Instant::now());
    match change {
        PeerChange::New => publish(bus, DiscoveryEvent::PeerFound(merged)),
        PeerChange::Updated => publish(bus, DiscoveryEvent::PeerUpdated(merged)),
        PeerChange::Refreshed => {}
    }
}

/// Locks the peer table, recovering from a poisoned mutex.
///
/// The table is a single `BTreeMap`; a panic elsewhere cannot leave it
/// half-updated, so serving the snapshot is strictly better than failing.
fn lock_table(peers: &Mutex<PeerTable>) -> MutexGuard<'_, PeerTable> {
    peers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs the BLE tier: attaches the backend, watches it, restarts on failure.
async fn ble_supervisor(
    backend: Arc<dyn BleBackend>,
    request: BleRequest,
    bus: EventBus,
    observations: mpsc::UnboundedSender<Observation>,
    backoff: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    publish_state(&bus, TierId::Ble, TierState::Starting);
    let sink: Arc<dyn BleObservationSink> = Arc::new(BleSink { observations });
    let health_interval = backoff.max(Duration::from_secs(1));

    loop {
        if *shutdown.borrow() {
            break;
        }
        match backend.start(request, Arc::clone(&sink)) {
            Ok(()) => {
                publish_state(&bus, TierId::Ble, TierState::Active);
                // Wait for shutdown or for the platform to drop the radio.
                loop {
                    tokio::select! {
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                break;
                            }
                        }
                        _ = tokio::time::sleep(health_interval) => {
                            if !backend.is_running() {
                                publish_state(
                                    &bus,
                                    TierId::Ble,
                                    TierState::Degraded("ble backend detached; restarting"),
                                );
                                backend.stop();
                                break;
                            }
                        }
                    }
                }
                if *shutdown.borrow() {
                    break;
                }
                if !pause(&mut shutdown, backoff).await {
                    break;
                }
            }
            Err(error) => {
                publish_error(&bus, TierId::Ble, error);
                publish_state(
                    &bus,
                    TierId::Ble,
                    TierState::Degraded("ble backend failed to start; retrying"),
                );
                if !pause(&mut shutdown, backoff).await {
                    break;
                }
            }
        }
    }

    backend.stop();
    publish_state(&bus, TierId::Ble, TierState::Stopped);
}

/// Sleeps for `duration`, returning `false` if shutdown interrupts the wait.
async fn pause(shutdown: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(duration) => true,
        changed = shutdown.changed() => changed.is_err() || *shutdown.borrow(),
    }
}

/// Forwards BLE sightings into the coordinator channel.
struct BleSink {
    observations: mpsc::UnboundedSender<Observation>,
}

impl BleObservationSink for BleSink {
    fn observe(&self, observation: BleObservation) {
        let peer = PeerInfo {
            id: observation.peer_id,
            name: None,
            // A BLE beacon carries no address; the same peer gains one when
            // mDNS or the subnet scanner observes the same id.
            addresses: Vec::new(),
        };
        let _ = self.observations.send(Observation::Ble(peer));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use crate::peer::PeerId;

    use super::*;

    #[test]
    fn ble_observations_have_no_address() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = BleSink { observations: tx };
        sink.observe(BleObservation {
            peer_id: PeerId::from_bytes([9; 16]),
            port: 42517,
        });
        match rx.try_recv().unwrap() {
            Observation::Ble(peer) => {
                assert_eq!(peer.id, PeerId::from_bytes([9; 16]));
                assert!(peer.addresses.is_empty());
            }
            other => panic!("expected a ble observation, got {other:?}"),
        }
    }
}
