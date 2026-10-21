//! AirBridge peer discovery.
//!
//! Three tiers, in escalating order of intrusiveness:
//!
//! | tier | mechanism | finds | cost |
//! |------|-----------|-------|------|
//! | 1 | BLE advertising/scanning on service UUID `0xFE2C` | devices in radio range, in milliseconds | milliseconds, no address |
//! | 2 | mDNS/DNS-SD `_airbridge._tcp.local.` | devices on the same link | one multicast socket |
//! | 3 | HTTP probe of every host on the local `/24` | devices whose multicast is filtered | one packet per host |
//!
//! Tiers 1 and 2 are passive: they advertise this node and listen. Tier 3 is
//! active and only runs when configured ([`SubnetPolicy::Always`]) or as a
//! fallback when mDNS fails or stays silent past a grace period
//! ([`SubnetPolicy::Fallback`], the default). All three feed one
//! [`PeerInfo`] table keyed by a 16-byte [`PeerId`], so a machine seen on BLE
//! and again on mDNS is one peer with two observations, not two peers.
//!
//! # Addresses are a merge, not a requirement
//!
//! A BLE beacon identifies a peer but carries no IP address (the legacy
//! advertisement has no room for both a full id and an address). Such a peer
//! enters the table with an empty address list and gains addresses when mDNS
//! or the subnet tier observes the same [`PeerId`]. An application that wants
//! to connect immediately can treat an address-less peer as "nearby but not
//! reachable yet" and wait for a `PeerUpdated` event.
//!
//! # Quick start
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use libcore_discovery::{
//!     DiscoveryConfig, DiscoveryCoordinator, DiscoveryEvent, MockBleBackend, PeerId,
//!     SubnetPolicy,
//! };
//!
//! # async fn example() -> Result<(), libcore_discovery::DiscoveryError> {
//! let config = DiscoveryConfig::new(PeerId::generate()?, 42517)
//!     .with_name("laptop")
//!     .with_subnet_policy(SubnetPolicy::Fallback);
//!
//! // `None` means "no platform BLE binding on this host".
//! let handle = DiscoveryCoordinator::start(config, Some(Arc::new(MockBleBackend::new())))?;
//! let mut events = handle.events();
//! while let Ok(event) = events.recv().await {
//!     if let DiscoveryEvent::PeerFound(peer) = event {
//!         println!("found {} at {:?}", peer.id, peer.addresses);
//!         break;
//!     }
//! }
//! handle.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! # Design notes
//!
//! * **One writer.** Tier tasks only push observations; the coordinator owns
//!   the peer table and the event ordering. See [`DiscoveryCoordinator`].
//! * **No platform BLE code.** [`BleBackend`] is the seam a platform binding
//!   implements; [`MockBleBackend`] makes the full lifecycle testable on any
//!   host.
//! * **mDNS owns its socket.** hickory-proto supplies the wire format, not a
//!   resolver. The browser is a responder too, because clients must answer
//!   queries for their own instance.
//! * **Fallback is automatic.** A failed mDNS tier escalates immediately; a
//!   silent one escalates after [`DiscoveryConfig::mdns_grace`].
//! * **No `unsafe`.** Forbidden at the workspace level.
//!
//! # Runtime
//!
//! This crate is Tokio-only, unlike the runtime-agnostic `libcore_crypto`. It
//! needs `rt`, `net`, `sync` and `time`. Its `rust-version` is higher than the
//! workspace floor because hickory-proto 0.26 requires a recent compiler;
//! the floors are declared per crate so the crypto core keeps its own.

mod ble;
mod config;
mod coordinator;
mod error;
mod event;
mod iface;
mod mdns;
mod peer;
mod subnet;

pub use ble::{
    BleBackend, BleObservation, BleObservationSink, BleRequest, MockBleBackend, PeerBeacon,
    BEACON_LEN, BEACON_VERSION,
};
pub use config::{
    DiscoveryConfig, Ipv4Subnet, MdnsConfig, SubnetPolicy, SubnetScanConfig, BLE_SERVICE_UUID,
    DEFAULT_BLE_RETRY_BACKOFF, DEFAULT_DISCOVERY_PORT, DEFAULT_MDNS_GRACE,
    DEFAULT_MDNS_QUERY_INTERVAL, DEFAULT_PEER_TTL, DEFAULT_SCAN_CONCURRENCY,
    DEFAULT_SWEEP_INTERVAL, MAX_PROBE_TIMEOUT, MDNS_MULTICAST_ADDR, MDNS_PORT,
    MDNS_RECORD_TTL_SECS, MDNS_SERVICE_TYPE, MIN_SUBNET_PREFIX, PEER_ID_HEADER, PEER_NAME_HEADER,
    PEER_PORT_HEADER, PROBE_USER_AGENT, REGISTER_PATH, SUBNET_PROBE_TIMEOUT,
};
pub use coordinator::{DiscoveryCoordinator, DiscoveryHandle};
pub use error::DiscoveryError;
pub use event::{DiscoveryEvent, TierId, TierState};
pub use peer::{PeerId, PeerInfo, PEER_ID_HEX_LEN, PEER_ID_LEN};
