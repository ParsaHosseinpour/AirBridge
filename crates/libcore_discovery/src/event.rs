//! Events emitted by the discovery coordinator.
//!
//! The coordinator never calls back into application code: it publishes
//! [`DiscoveryEvent`]s on a broadcast channel and the application decides how
//! to react. That keeps discovery free of UI or policy assumptions — a CLI
//! can print events, a GUI can fold them into a peer list, and a test can
//! assert on them.
//!
//! Events are lossy for slow subscribers by design: a peer list is
//! reconstructible from [`crate::DiscoveryHandle::peers`] at any time, so a
//! dropped intermediate event is never fatal. Terminal events (`Shutdown`,
//! `Stopped` tier states) are the exception a caller should wait for.

use std::fmt;
use std::sync::Arc;

use crate::error::DiscoveryError;
use crate::peer::PeerId;
use crate::peer::PeerInfo;

/// One of the three discovery tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TierId {
    /// Tier 1: BLE background advertisement and scan on service UUID
    /// `0xFE2C`.
    Ble,

    /// Tier 2: mDNS/DNS-SD registration and browsing.
    Mdns,

    /// Tier 3: active IPv4 subnet probe, either as the configured primary or
    /// as the fallback when mDNS cannot find anyone.
    Subnet,
}

impl TierId {
    /// Every tier, in escalation order.
    pub const ALL: [Self; 3] = [Self::Ble, Self::Mdns, Self::Subnet];
}

impl fmt::Display for TierId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ble => "ble",
            Self::Mdns => "mdns",
            Self::Subnet => "subnet",
        })
    }
}

/// Lifecycle state of a single tier.
#[derive(Clone, Debug)]
pub enum TierState {
    /// The tier is coming up: binding sockets, attaching the BLE backend, or
    /// sending the first query.
    Starting,

    /// The tier is running and expected to produce peers.
    Active,

    /// The tier is running but diminished, with a human-readable reason.
    ///
    /// Example: mDNS could only join the multicast group on some interfaces.
    Degraded(&'static str),

    /// The tier stopped or failed to start.
    ///
    /// A failed mDNS tier with [`crate::SubnetPolicy::Fallback`] is exactly
    /// the condition that escalates to an active subnet scan.
    Failed(Arc<DiscoveryError>),

    /// The tier was shut down cleanly, usually because the coordinator is
    /// dropping.
    Stopped,
}

impl TierState {
    /// Returns whether the state is `Failed`.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    /// Returns whether the tier will not run again without a restart.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Failed(_) | Self::Stopped)
    }
}

/// Something worth telling the application about.
///
/// `PeerFound` / `PeerUpdated` / `PeerLost` are the deduplicated view: because
/// the coordinator keys peers by [`PeerId`], a machine advertising over BLE
/// *and* answering mDNS produces one `PeerFound` and, if the second source
/// adds an address or a name, one `PeerUpdated`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum DiscoveryEvent {
    /// A peer entered the peer table.
    PeerFound(PeerInfo),

    /// A known peer gained a name or an address.
    PeerUpdated(PeerInfo),

    /// A peer's last observation aged past the table's time-to-live and it was
    /// evicted.
    PeerLost(PeerId),

    /// A tier changed lifecycle state.
    TierStateChanged {
        /// The tier that changed.
        tier: TierId,
        /// Its new state.
        state: TierState,
    },

    /// A tier reported a non-fatal error.
    ///
    /// Individual probe failures, UDP receive errors and undecodable messages
    /// surface here without tearing the tier down.
    TierError {
        /// The tier that reported the error.
        tier: TierId,
        /// The error itself.
        error: Arc<DiscoveryError>,
    },

    /// A subnet scan started, covering `subnets` networks.
    SubnetScanStarted {
        /// Number of networks in the sweep.
        subnets: usize,
    },

    /// A subnet scan completed.
    SubnetScanFinished {
        /// Number of host addresses probed.
        probed: usize,
        /// Number of hosts that answered the probe.
        responded: usize,
        /// Number of those responses that produced a peer entry.
        discovered: usize,
    },

    /// The coordinator shut down all tiers and stopped emitting events.
    Shutdown,
}

impl DiscoveryEvent {
    /// Returns the tier the event belongs to, when it is tier-scoped.
    #[must_use]
    pub fn tier(&self) -> Option<TierId> {
        match self {
            Self::TierStateChanged { tier, .. } | Self::TierError { tier, .. } => Some(*tier),
            Self::SubnetScanStarted { .. } | Self::SubnetScanFinished { .. } => {
                Some(TierId::Subnet)
            }
            _ => None,
        }
    }
}

/// Broadcast sender through which every tier publishes its events.
///
/// The coordinator owns one sender and hands clones to its tier tasks. A send
/// with no subscribers is not an error — events are a best-effort mirror of
/// the peer table, which remains authoritative.
pub(crate) type EventBus = tokio::sync::broadcast::Sender<DiscoveryEvent>;

/// Publishes an event, ignoring the absence of subscribers.
pub(crate) fn publish(bus: &EventBus, event: DiscoveryEvent) {
    let _ = bus.send(event);
}

/// Publishes a tier lifecycle transition.
pub(crate) fn publish_state(bus: &EventBus, tier: TierId, state: TierState) {
    publish(bus, DiscoveryEvent::TierStateChanged { tier, state });
}

/// Publishes a non-fatal tier error.
pub(crate) fn publish_error(bus: &EventBus, tier: TierId, error: DiscoveryError) {
    publish(
        bus,
        DiscoveryEvent::TierError {
            tier,
            error: Arc::new(error),
        },
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn tier_ids_have_stable_names() {
        assert_eq!(TierId::Ble.to_string(), "ble");
        assert_eq!(TierId::Mdns.to_string(), "mdns");
        assert_eq!(TierId::Subnet.to_string(), "subnet");
        assert_eq!(TierId::ALL.len(), 3);
    }

    #[test]
    fn tier_state_helpers_classify() {
        assert!(TierState::Failed(Arc::new(DiscoveryError::NoUsableInterface)).is_failed());
        assert!(TierState::Failed(Arc::new(DiscoveryError::NoUsableInterface)).is_terminal());
        assert!(TierState::Stopped.is_terminal());
        assert!(!TierState::Starting.is_terminal());
        assert!(!TierState::Active.is_failed());
    }

    #[test]
    fn events_reference_their_tier() {
        let event = DiscoveryEvent::TierError {
            tier: TierId::Mdns,
            error: Arc::new(DiscoveryError::NoUsableInterface),
        };
        assert_eq!(event.tier(), Some(TierId::Mdns));
        assert_eq!(DiscoveryEvent::Shutdown.tier(), None);
        assert_eq!(
            DiscoveryEvent::SubnetScanStarted { subnets: 1 }.tier(),
            Some(TierId::Subnet)
        );
    }
}
