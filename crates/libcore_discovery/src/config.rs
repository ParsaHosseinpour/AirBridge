//! Discovery configuration: ports, intervals, policies and the tier-3 subnet
//! geometry.
//!
//! Everything the coordinator needs is a plain value in [`DiscoveryConfig`],
//! so a daemon can parse it from flags and a test can build it inline. The
//! setters are chainable and infallible; validation is a separate step
//! ([`DiscoveryConfig::validate`]) so configuration can be assembled in any
//! order.

use std::fmt;
use std::net::Ipv4Addr;
use std::time::Duration;

use crate::error::DiscoveryError;
use crate::peer::PeerId;

/// The BLE service UUID AirBridge advertises and scans for.
///
/// `0xFE2C` is the 16-bit UUID the Bluetooth SIG assigned to Fast Pair; we
/// reuse the *value* as a discovery beacon namespace. That is a
/// deliberate trade: any off-the-shelf scanner can see that an AirBridge is
/// nearby, and we get 128-bit-UUID-shaped interoperability for free, at the
/// cost of not owning the identifier. The beacon payload carries an AirBridge
/// version byte, so a foreign 0xFE2C advertiser is rejected, not misparsed.
pub const BLE_SERVICE_UUID: u16 = 0xFE2C;

/// DNS-SD service type registered and browsed over mDNS.
pub const MDNS_SERVICE_TYPE: &str = "_airbridge._tcp.local.";

/// IPv4 multicast group and port for mDNS (RFC 6762 section 3).
pub const MDNS_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);

/// mDNS UDP port; queries and responses share it.
pub const MDNS_PORT: u16 = 5353;

/// TTL, in seconds, placed on the records we announce.
///
/// Peers treat a record older than this as gone. Combined with the periodic
/// re-announcement below, a cleanly stopped peer disappears within at most
/// this long even if its goodbye packet is lost.
pub const MDNS_RECORD_TTL_SECS: u32 = 120;

/// HTTP path probed on every host of a subnet scan.
///
/// A peer answers a plain `GET` on this path with 2xx and identification
/// headers; anything else (refused, timeout, 404) means "no peer here" and is
/// not an error worth surfacing.
pub const REGISTER_PATH: &str = "/api/v1/register";

/// Response header carrying the peer's identifier, as 32 hex digits.
pub const PEER_ID_HEADER: &str = "x-airbridge-peer-id";

/// Response header carrying the peer's display name, as UTF-8.
pub const PEER_NAME_HEADER: &str = "x-airbridge-name";

/// Response header carrying the peer's serving port.
pub const PEER_PORT_HEADER: &str = "x-airbridge-port";

/// User agent sent with subnet probes.
pub const PROBE_USER_AGENT: &str = concat!("airbridge-discovery/", env!("CARGO_PKG_VERSION"));

/// Strict per-probe deadline for the tier-3 scanner.
///
/// The value is deliberately aggressive. On a local link an RTT is
/// sub-millisecond; 1.5 seconds is four orders of magnitude of headroom. A
/// host that needs longer is a host we would wait a week for once a `/24` is
/// swept in parallel, and a *silent* host (firewalled) is indistinguishable
/// from a slow one anyway.
pub const SUBNET_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Default TCP port peers are expected to serve discovery answers on.
///
/// This is the value printed by the reference application and the port the
/// native channel ends up on; the scanner probes it on every host.
pub const DEFAULT_DISCOVERY_PORT: u16 = 42517;

/// How long a peer may go unobserved before it is evicted from the table.
pub const DEFAULT_PEER_TTL: Duration = Duration::from_secs(60);

/// How often the coordinator re-announces over mDNS and re-queries the link.
pub const DEFAULT_MDNS_QUERY_INTERVAL: Duration = Duration::from_secs(5);

/// How often the coordinator sweeps the peer table for expired entries.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// How long mDNS gets to produce a peer before the fallback scan runs.
pub const DEFAULT_MDNS_GRACE: Duration = Duration::from_secs(15);

/// How long the BLE coordinator waits between failed backend restarts.
pub const DEFAULT_BLE_RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// Maximum number of subnet probes in flight at once.
pub const DEFAULT_SCAN_CONCURRENCY: usize = 128;

/// Smallest IPv4 subnet the scanner accepts.
pub const MIN_SUBNET_PREFIX: u8 = 16;

/// Controls whether and when tier 3 runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubnetPolicy {
    /// Never probe the subnet. For wired test rigs and hosts whose network
    /// administrator would notice a `/24` sweep.
    Disabled,

    /// Probe only when mDNS failed or found nobody within the grace period.
    /// This is the default: the passive tiers get first say.
    #[default]
    Fallback,

    /// Probe immediately on start, in parallel with the other tiers.
    Always,
}

impl fmt::Display for SubnetPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Disabled => "disabled",
            Self::Fallback => "fallback",
            Self::Always => "always",
        })
    }
}

/// An IPv4 network in CIDR notation that the scanner knows how to enumerate.
///
/// Only networks between `/16` and `/32` are accepted. The lower bound keeps a
/// misconfiguration (a `/8` is 16 million probes) from turning a laptop into a
/// network scanner; the upper bound covers loopback test rigs and
/// point-to-point links.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ipv4Subnet {
    network: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Subnet {
    /// Creates a subnet, requiring `network` to have all host bits zero.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoveryError::InvalidConfig`] when the prefix is outside
    /// `16..=32` or the address is not the masked network address.
    pub fn new(network: Ipv4Addr, prefix: u8) -> Result<Self, DiscoveryError> {
        if !(MIN_SUBNET_PREFIX..=32).contains(&prefix) {
            return Err(DiscoveryError::InvalidConfig(
                "subnet prefix must be between 16 and 32",
            ));
        }
        if mask(network, prefix) != network {
            return Err(DiscoveryError::InvalidConfig(
                "subnet address has host bits set; pass the network address",
            ));
        }
        Ok(Self { network, prefix })
    }

    /// Returns the `/24` containing `host`.
    ///
    /// Used to turn a local interface address into the network worth
    /// probing: the tier-3 scan is defined over `/24`s, so a host on
    /// `192.168.7.34` implies a sweep of `192.168.7.0/24`.
    #[must_use]
    pub fn from_host(host: Ipv4Addr) -> Self {
        Self {
            network: mask(host, 24),
            prefix: 24,
        }
    }

    /// The masked network address.
    #[must_use]
    pub const fn network(&self) -> Ipv4Addr {
        self.network
    }

    /// The prefix length in bits.
    #[must_use]
    pub const fn prefix(&self) -> u8 {
        self.prefix
    }

    /// Iterates the addresses worth probing, excluding the network and
    /// broadcast addresses.
    ///
    /// `/31` and `/32` have no reserved pair (RFC 3021), so every address is
    /// yielded. Order is ascending and stable.
    pub fn hosts(&self) -> impl Iterator<Item = Ipv4Addr> {
        let host_bits = u32::from(32 - self.prefix);
        let base = u64::from(u32::from(self.network));
        let count = 1_u64 << host_bits;
        let (first, last) = match host_bits {
            0 => (base, base),
            1 => (base, base + count - 1),
            _ => (base + 1, base + count - 2),
        };
        (first..=last).map(|host| Ipv4Addr::from(host as u32))
    }
}

impl fmt::Display for Ipv4Subnet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

/// Applies a prefix mask to an address.
fn mask(address: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    let bits = u32::from(address);
    let masked = if prefix == 0 {
        0
    } else {
        bits & (!0_u32 << (32 - prefix))
    };
    Ipv4Addr::from(masked)
}

/// Tunables for a subnet scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubnetScanConfig {
    /// Networks to sweep. When empty, the coordinator derives a `/24` from
    /// each local IPv4 interface address at scan time.
    pub subnets: Vec<Ipv4Subnet>,

    /// TCP port probed on every host.
    pub port: u16,

    /// Maximum probes in flight at once.
    pub concurrency: usize,

    /// Per-probe deadline.
    pub timeout: Duration,
}

impl Default for SubnetScanConfig {
    fn default() -> Self {
        Self {
            subnets: Vec::new(),
            port: DEFAULT_DISCOVERY_PORT,
            concurrency: DEFAULT_SCAN_CONCURRENCY,
            timeout: SUBNET_PROBE_TIMEOUT,
        }
    }
}

impl SubnetScanConfig {
    /// Creates the default scan configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the networks to sweep.
    #[must_use]
    pub fn with_subnets(mut self, subnets: Vec<Ipv4Subnet>) -> Self {
        self.subnets = subnets;
        self
    }

    /// Sets the probed port.
    #[must_use]
    pub const fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Sets the maximum number of concurrent probes.
    #[must_use]
    pub const fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency;
        self
    }

    /// Sets the per-probe deadline.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Tunables for the mDNS tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdnsConfig {
    /// DNS-SD service type to register and browse.
    pub service_type: String,

    /// How often announcements are re-sent and the service is re-queried.
    pub query_interval: Duration,

    /// Interface address to bind the multicast socket to.
    ///
    /// `None` binds the wildcard address and joins on the system default
    /// interface, which is right for the common single-uplink case.
    pub interface: Option<Ipv4Addr>,
}

impl Default for MdnsConfig {
    fn default() -> Self {
        Self {
            service_type: MDNS_SERVICE_TYPE.to_owned(),
            query_interval: DEFAULT_MDNS_QUERY_INTERVAL,
            interface: None,
        }
    }
}

impl MdnsConfig {
    /// Creates the default mDNS configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the browsed service type.
    #[must_use]
    pub fn with_service_type(mut self, service_type: impl Into<String>) -> Self {
        self.service_type = service_type.into();
        self
    }

    /// Sets the announcement and query interval.
    #[must_use]
    pub const fn with_query_interval(mut self, interval: Duration) -> Self {
        self.query_interval = interval;
        self
    }

    /// Pins the socket to one interface address.
    #[must_use]
    pub const fn with_interface(mut self, interface: Ipv4Addr) -> Self {
        self.interface = Some(interface);
        self
    }
}

/// The full discovery configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryConfig {
    /// This node's identifier, repeated in every advertisement.
    pub peer_id: PeerId,

    /// Name advertised to other nodes; `None` advertises the id alone.
    pub peer_name: Option<String>,

    /// Port other nodes should connect to for file transfer.
    ///
    /// This is advertised over all three tiers; it is not the port of the
    /// discovery listener itself (which is the subnet scan's HTTP probe and
    /// is configured in [`SubnetScanConfig::port`]).
    pub listen_port: u16,

    /// How long a peer survives without a new observation.
    pub peer_ttl: Duration,

    /// Whether tier 1 is started at all.
    pub ble_enabled: bool,

    /// Whether tier 2 is started at all.
    ///
    /// Disabling mDNS is useful for tests, for hosts whose network policy
    /// forbids multicast, and for applications that only want BLE or the
    /// active scan.
    pub mdns_enabled: bool,

    /// Tier 2 tunables.
    pub mdns: MdnsConfig,

    /// Tier 3 policy.
    pub subnet_policy: SubnetPolicy,

    /// Tier 3 tunables.
    pub subnet_scan: SubnetScanConfig,

    /// How long mDNS may run without finding anyone before a
    /// [`SubnetPolicy::Fallback`] scan is triggered.
    pub mdns_grace: Duration,

    /// How often expired peers are swept out of the table.
    pub sweep_interval: Duration,

    /// How long the BLE tier waits before retrying a failed backend.
    pub ble_retry_backoff: Duration,
}

impl DiscoveryConfig {
    /// Creates a configuration advertising `peer_id` on `listen_port`.
    ///
    /// Everything else takes its default; the sensible chain is
    /// [`Self::with_name`] and then [`Self::validate`].
    #[must_use]
    pub fn new(peer_id: PeerId, listen_port: u16) -> Self {
        Self {
            peer_id,
            peer_name: None,
            listen_port,
            peer_ttl: DEFAULT_PEER_TTL,
            ble_enabled: true,
            mdns_enabled: true,
            mdns: MdnsConfig::default(),
            subnet_policy: SubnetPolicy::default(),
            subnet_scan: SubnetScanConfig::default(),
            mdns_grace: DEFAULT_MDNS_GRACE,
            sweep_interval: DEFAULT_SWEEP_INTERVAL,
            ble_retry_backoff: DEFAULT_BLE_RETRY_BACKOFF,
        }
    }

    /// Sets the advertised name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.peer_name = Some(name.into());
        self
    }

    /// Sets the peer time-to-live.
    #[must_use]
    pub const fn with_peer_ttl(mut self, ttl: Duration) -> Self {
        self.peer_ttl = ttl;
        self
    }

    /// Enables or disables the BLE tier.
    #[must_use]
    pub const fn with_ble(mut self, enabled: bool) -> Self {
        self.ble_enabled = enabled;
        self
    }

    /// Enables or disables the mDNS tier.
    #[must_use]
    pub const fn with_mdns_enabled(mut self, enabled: bool) -> Self {
        self.mdns_enabled = enabled;
        self
    }

    /// Sets the mDNS tier configuration.
    #[must_use]
    pub fn with_mdns(mut self, mdns: MdnsConfig) -> Self {
        self.mdns = mdns;
        self
    }

    /// Sets the subnet policy.
    #[must_use]
    pub const fn with_subnet_policy(mut self, policy: SubnetPolicy) -> Self {
        self.subnet_policy = policy;
        self
    }

    /// Sets the subnet scan configuration.
    #[must_use]
    pub fn with_subnet_scan(mut self, scan: SubnetScanConfig) -> Self {
        self.subnet_scan = scan;
        self
    }

    /// Sets the mDNS grace period before fallback.
    #[must_use]
    pub const fn with_mdns_grace(mut self, grace: Duration) -> Self {
        self.mdns_grace = grace;
        self
    }

    /// Sets the table sweep interval.
    #[must_use]
    pub const fn with_sweep_interval(mut self, interval: Duration) -> Self {
        self.sweep_interval = interval;
        self
    }

    /// Sets the BLE backend retry backoff.
    #[must_use]
    pub const fn with_ble_retry_backoff(mut self, backoff: Duration) -> Self {
        self.ble_retry_backoff = backoff;
        self
    }

    /// Checks the configuration for values that cannot work.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoveryError::InvalidConfig`] for a zero TTL, interval or
    /// concurrency, or a probe timeout beyond [`MAX_PROBE_TIMEOUT`].
    pub fn validate(&self) -> Result<(), DiscoveryError> {
        if self.listen_port == 0 {
            return Err(DiscoveryError::InvalidConfig(
                "listen port must be non-zero",
            ));
        }
        if self.peer_ttl.is_zero() {
            return Err(DiscoveryError::InvalidConfig("peer ttl must be non-zero"));
        }
        if self.sweep_interval.is_zero() {
            return Err(DiscoveryError::InvalidConfig(
                "sweep interval must be non-zero",
            ));
        }
        if self.mdns.query_interval.is_zero() {
            return Err(DiscoveryError::InvalidConfig(
                "mdns query interval must be non-zero",
            ));
        }
        if self.ble_retry_backoff.is_zero() {
            return Err(DiscoveryError::InvalidConfig(
                "ble retry backoff must be non-zero",
            ));
        }
        if self.subnet_scan.concurrency == 0 {
            return Err(DiscoveryError::InvalidConfig(
                "subnet scan concurrency must be non-zero",
            ));
        }
        if self.subnet_scan.timeout > MAX_PROBE_TIMEOUT {
            return Err(DiscoveryError::InvalidConfig(
                "subnet probe timeout exceeds the 10 second ceiling",
            ));
        }
        if self.subnet_scan.port == 0 {
            return Err(DiscoveryError::InvalidConfig(
                "subnet probe port must be non-zero",
            ));
        }
        if self.mdns.service_type.is_empty() {
            return Err(DiscoveryError::InvalidConfig(
                "mdns service type must not be empty",
            ));
        }
        Ok(())
    }
}

/// Hard ceiling on the configured probe timeout.
///
/// A subnet sweep must stay a background activity; a caller that wants to wait
/// ten seconds per host has configured a port scanner, not discovery.
pub const MAX_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn subnet_enumerates_usable_hosts() {
        let subnet = Ipv4Subnet::from_host(Ipv4Addr::new(192, 168, 7, 34));
        assert_eq!(subnet.network(), Ipv4Addr::new(192, 168, 7, 0));
        assert_eq!(subnet.prefix(), 24);
        let hosts: Vec<Ipv4Addr> = subnet.hosts().collect();
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 7, 1));
        assert_eq!(hosts[253], Ipv4Addr::new(192, 168, 7, 254));
    }

    #[test]
    fn point_to_point_prefixes_exclude_nothing() {
        let pair = Ipv4Subnet::new(Ipv4Addr::new(10, 0, 0, 4), 31).unwrap();
        assert_eq!(
            pair.hosts().collect::<Vec<_>>(),
            vec![Ipv4Addr::new(10, 0, 0, 4), Ipv4Addr::new(10, 0, 0, 5)]
        );
        let single = Ipv4Subnet::new(Ipv4Addr::new(10, 0, 0, 9), 32).unwrap();
        assert_eq!(
            single.hosts().collect::<Vec<_>>(),
            vec![Ipv4Addr::new(10, 0, 0, 9)]
        );
    }

    #[test]
    fn small_subnets_exclude_network_and_broadcast() {
        let subnet = Ipv4Subnet::new(Ipv4Addr::new(127, 0, 0, 0), 28).unwrap();
        let hosts: Vec<Ipv4Addr> = subnet.hosts().collect();
        assert_eq!(hosts.first(), Some(&Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(hosts.last(), Some(&Ipv4Addr::new(127, 0, 0, 14)));
        assert_eq!(hosts.len(), 14);
    }

    #[test]
    fn subnet_validation_rejects_bad_geometry() {
        assert!(Ipv4Subnet::new(Ipv4Addr::new(192, 168, 1, 5), 24).is_err());
        assert!(Ipv4Subnet::new(Ipv4Addr::new(192, 168, 0, 0), 15).is_err());
        assert!(Ipv4Subnet::new(Ipv4Addr::new(193, 168, 0, 0), 24).is_ok());
    }

    #[test]
    fn default_config_passes_validation() {
        let config = DiscoveryConfig::new(PeerId::from_bytes([3; 16]), 42517).with_name("laptop");
        config.validate().unwrap();
        assert_eq!(config.subnet_policy, SubnetPolicy::Fallback);
        assert_eq!(config.mdns.service_type, MDNS_SERVICE_TYPE);
        assert_eq!(config.subnet_scan.timeout, SUBNET_PROBE_TIMEOUT);
    }

    #[test]
    fn zero_knobs_fail_validation() {
        let base = PeerId::from_bytes([3; 16]);
        assert!(DiscoveryConfig::new(base, 1)
            .with_peer_ttl(Duration::ZERO)
            .validate()
            .is_err());
        assert!(DiscoveryConfig::new(base, 1)
            .with_sweep_interval(Duration::ZERO)
            .validate()
            .is_err());
        assert!(DiscoveryConfig::new(base, 0).validate().is_err());
        assert!(DiscoveryConfig::new(base, 1)
            .with_subnet_scan(SubnetScanConfig::new().with_concurrency(0))
            .validate()
            .is_err());
        assert!(DiscoveryConfig::new(base, 1)
            .with_subnet_scan(SubnetScanConfig::new().with_timeout(Duration::from_secs(11)))
            .validate()
            .is_err());
    }

    #[test]
    fn subnet_policy_display_is_stable() {
        assert_eq!(SubnetPolicy::Disabled.to_string(), "disabled");
        assert_eq!(SubnetPolicy::Fallback.to_string(), "fallback");
        assert_eq!(SubnetPolicy::Always.to_string(), "always");
        assert_eq!(
            Ipv4Subnet::new(Ipv4Addr::new(10, 1, 0, 0), 16)
                .unwrap()
                .to_string(),
            "10.1.0.0/16"
        );
    }
}
