//! Peer identity and the in-memory peer table.
//!
//! A [`PeerId`] is 16 opaque bytes. Nothing in this crate derives trust from
//! it: it exists so that the same machine advertising over BLE, answering on
//! mDNS and replying to a subnet probe collapses into one row in the table
//! instead of three. Peers that have no cryptographic identity yet (a host
//! that only answered an HTTP probe) get a *derived* id from their address, so
//! they are at least stable within a network.
//!
//! # Freshness
//!
//! Every tier can observe the same peer repeatedly, so the table timestamps
//! each observation and evicts rows whose last contact is older than the
//! configured time-to-live. Eviction is driven by the coordinator's sweep
//! tick, not by a timer per peer: one clock, one place to reason about it.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};

use crate::error::DiscoveryError;

/// Length of a peer identifier in bytes.
pub const PEER_ID_LEN: usize = 16;

/// Number of hex digits in the textual form of a peer identifier.
pub const PEER_ID_HEX_LEN: usize = PEER_ID_LEN * 2;

/// Domain separator for address-derived peer identifiers.
const PEER_ID_CONTEXT: &[u8] = b"airbridge/peer-id/v1";

/// Stable, opaque identifier for a discoverable AirBridge peer.
///
/// The id is generated once per process and reused for every tier, which is
/// what lets the coordinator merge the same machine discovered over BLE,
/// mDNS and the subnet scan.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId([u8; PEER_ID_LEN]);

impl PeerId {
    /// Wraps raw identifier bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; PEER_ID_LEN]) -> Self {
        Self(bytes)
    }

    /// Returns the raw identifier bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; PEER_ID_LEN] {
        &self.0
    }

    /// Draws a fresh random identifier from the system entropy source.
    ///
    /// # Errors
    ///
    /// Fails if the platform random number generator is unavailable.
    pub fn generate() -> Result<Self, DiscoveryError> {
        let mut bytes = [0_u8; PEER_ID_LEN];
        SystemRandom::new().fill(&mut bytes).map_err(|_| {
            DiscoveryError::Io(std::io::Error::other(
                "system random number generator is unavailable",
            ))
        })?;
        Ok(Self(bytes))
    }

    /// Derives the identifier of a peer that has not revealed one.
    ///
    /// The id is `SHA-256(context || family || address || port)`, truncated to
    /// 16 bytes. It is not a security boundary — an attacker can trivially
    /// claim the id of a host it is not — but it makes an anonymous responder
    /// (a subnet-probe hit without headers) collapse into the same table row
    /// across scans and remain distinct from every other host.
    #[must_use]
    pub fn derive_from(address: &SocketAddr) -> Self {
        let mut context = digest::Context::new(&digest::SHA256);
        context.update(PEER_ID_CONTEXT);
        match address {
            SocketAddr::V4(v4) => {
                context.update(&[4]);
                context.update(&v4.ip().octets());
                context.update(&v4.port().to_be_bytes());
            }
            SocketAddr::V6(v6) => {
                context.update(&[6]);
                context.update(&v6.ip().octets());
                context.update(&v6.port().to_be_bytes());
            }
        }
        let digest = context.finish();
        let mut bytes = [0_u8; PEER_ID_LEN];
        bytes.copy_from_slice(&digest.as_ref()[..PEER_ID_LEN]);
        Self(bytes)
    }

    /// Parses the 32-hex-digit textual form.
    ///
    /// Uppercase and lowercase digits are both accepted.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoveryError::Encoding`] when the input is not exactly
    /// [`PEER_ID_HEX_LEN`] hex digits.
    pub fn from_hex(text: &str) -> Result<Self, DiscoveryError> {
        if text.len() != PEER_ID_HEX_LEN {
            return Err(DiscoveryError::Encoding(
                "peer id must be exactly 32 hexadecimal digits",
            ));
        }
        let mut bytes = [0_u8; PEER_ID_LEN];
        for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
            let high = hex_digit(pair[0])
                .ok_or(DiscoveryError::Encoding("peer id contains a non-hex digit"))?;
            let low = hex_digit(pair[1])
                .ok_or(DiscoveryError::Encoding("peer id contains a non-hex digit"))?;
            *byte = (high << 4) | low;
        }
        Ok(Self(bytes))
    }

    /// Renders the identifier as 32 lowercase hex digits.
    #[must_use]
    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(PEER_ID_HEX_LEN);
        for byte in self.0 {
            text.push(char::from(HEX[usize::from(byte >> 4)]));
            text.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        text
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerId({self})")
    }
}

/// Decodes one ASCII hex digit.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Everything the discovery layer knows about one peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerInfo {
    /// Stable identifier, generated by the peer or derived from its address.
    pub id: PeerId,

    /// Human-readable name, when the peer advertised one.
    ///
    /// This is display data, not identity: a hostile peer can set any name it
    /// likes, so callers must not use it for trust decisions.
    pub name: Option<String>,

    /// Every address the peer has been observed on, sorted and deduplicated.
    ///
    /// A machine on Wi-Fi and Ethernet legitimately produces two entries; the
    /// connecting layer is expected to race or pick by policy.
    pub addresses: Vec<SocketAddr>,
}

impl PeerInfo {
    /// Creates a peer record with a single known address.
    #[must_use]
    pub fn new(id: PeerId, address: SocketAddr) -> Self {
        Self {
            id,
            name: None,
            addresses: vec![address],
        }
    }

    /// Sets the advertised name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Merges another observation of the same peer into this record.
    ///
    /// Returns `true` when the observable data (name or addresses) changed.
    /// Names are only ever *filled in*, never replaced: the first source to
    /// name a peer wins, which keeps an mDNS browse from flapping the display
    /// name that a BLE beacon already supplied.
    pub fn merge(&mut self, other: &Self) -> bool {
        let mut changed = false;
        if self.name.is_none() && other.name.is_some() {
            self.name.clone_from(&other.name);
            changed = true;
        }
        for address in &other.addresses {
            if !self.addresses.contains(address) {
                self.addresses.push(*address);
                changed = true;
            }
        }
        if changed {
            self.addresses.sort_unstable();
            self.addresses.dedup();
        }
        changed
    }
}

/// What an insertion into the [`PeerTable`] changed about the peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerChange {
    /// The peer was not in the table; a `PeerFound` is warranted.
    New,
    /// The peer was known but its name or addresses changed.
    Updated,
    /// The peer was known and unchanged; only its freshness moved.
    Refreshed,
}

/// One row of the [`PeerTable`]: the peer plus when it was last observed.
#[derive(Clone, Debug)]
struct PeerEntry {
    info: PeerInfo,
    last_seen: Instant,
}

/// Deduplicating, TTL-bounded peer registry.
///
/// Keys are [`PeerId`]s, so the table also serves as the deduplication point
/// for the coordinator: the same peer found by three tiers is one entry. The
/// `BTreeMap` keeps iteration order deterministic, which makes event sequences
/// reproducible in tests.
#[derive(Clone, Debug)]
pub(crate) struct PeerTable {
    entries: BTreeMap<PeerId, PeerEntry>,
}

impl PeerTable {
    /// Creates an empty table.
    pub(crate) const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Records an observation of `info` at `now` and classifies the change.
    ///
    /// Returns the change classification together with the merged record.
    pub(crate) fn insert(&mut self, info: PeerInfo, now: Instant) -> (PeerChange, PeerInfo) {
        match self.entries.get_mut(&info.id) {
            Some(entry) => {
                entry.last_seen = now;
                let changed = entry.info.merge(&info);
                let change = if changed {
                    PeerChange::Updated
                } else {
                    PeerChange::Refreshed
                };
                (change, entry.info.clone())
            }
            None => {
                self.entries.insert(
                    info.id,
                    PeerEntry {
                        info: info.clone(),
                        last_seen: now,
                    },
                );
                (PeerChange::New, info)
            }
        }
    }

    /// Drops every peer whose last observation is older than `ttl`.
    ///
    /// Returns the evicted ids in ascending order, so the coordinator can emit
    /// `PeerLost` events deterministically.
    pub(crate) fn expire(&mut self, now: Instant, ttl: Duration) -> Vec<PeerId> {
        let mut evicted = Vec::new();
        // `retain` cannot report the keys it drops; the sweep is over a
        // snapshot of ids, and `remove` makes the cost explicit.
        let stale: Vec<PeerId> = self
            .entries
            .iter()
            .filter(|(_, entry)| now.saturating_duration_since(entry.last_seen) >= ttl)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.entries.remove(&id);
            evicted.push(id);
        }
        evicted
    }

    /// Returns every live peer, ordered by id.
    pub(crate) fn snapshot(&self) -> Vec<PeerInfo> {
        self.entries
            .values()
            .map(|entry| entry.info.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn address(port: u16) -> SocketAddr {
        SocketAddr::from(([192, 168, 1, 20], port))
    }

    #[test]
    fn hex_roundtrip_is_case_insensitive() {
        let id = PeerId::from_bytes([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);
        let text = id.to_hex();
        assert_eq!(text, "00112233445566778899aabbccddeeff");
        assert_eq!(PeerId::from_hex(&text).unwrap(), id);
        assert_eq!(PeerId::from_hex(&text.to_uppercase()).unwrap(), id);
    }

    #[test]
    fn malformed_hex_is_rejected() {
        assert!(PeerId::from_hex("0011").is_err());
        assert!(PeerId::from_hex("00112233445566778899aabbccddeefg").is_err());
        assert!(PeerId::from_hex("").is_err());
    }

    #[test]
    fn generated_ids_are_distinct_and_not_derived_values() {
        let first = PeerId::generate().unwrap();
        let second = PeerId::generate().unwrap();
        assert_ne!(first, second);
        assert_ne!(first, PeerId::derive_from(&address(1)));
    }

    #[test]
    fn derived_ids_are_stable_and_address_sensitive() {
        let expected = PeerId::derive_from(&address(42));
        assert_eq!(expected, PeerId::derive_from(&address(42)));
        assert_ne!(expected, PeerId::derive_from(&address(43)));
        assert_ne!(
            PeerId::derive_from(&"192.168.1.20:42".parse().unwrap()),
            PeerId::derive_from(&"[fe80::1]:42".parse().unwrap())
        );
    }

    #[test]
    fn merge_fills_names_and_unions_addresses() {
        let id = PeerId::from_bytes([7; PEER_ID_LEN]);
        let mut info = PeerInfo::new(id, address(1));
        assert!(info.merge(&PeerInfo::new(id, address(2)).with_name("laptop")));
        assert_eq!(info.name.as_deref(), Some("laptop"));
        assert_eq!(info.addresses, vec![address(1), address(2)]);

        // A second name must not replace the first, and a known address is
        // not a change.
        assert!(!info.merge(&PeerInfo::new(id, address(1)).with_name("imposter")));
        assert_eq!(info.name.as_deref(), Some("laptop"));
        assert_eq!(info.addresses.len(), 2);
    }

    #[test]
    fn table_classifies_new_updated_and_refreshed() {
        let start = Instant::now();
        let mut table = PeerTable::new();
        let id = PeerId::from_bytes([1; PEER_ID_LEN]);

        let (change, merged) = table.insert(PeerInfo::new(id, address(1)), start);
        assert_eq!(change, PeerChange::New);
        assert_eq!(merged.addresses, vec![address(1)]);

        let (change, _) = table.insert(PeerInfo::new(id, address(1)), start);
        assert_eq!(change, PeerChange::Refreshed);

        let (change, merged) = table.insert(PeerInfo::new(id, address(2)), start);
        assert_eq!(change, PeerChange::Updated);
        assert_eq!(merged.addresses, vec![address(1), address(2)]);
        assert_eq!(table.snapshot().len(), 1);
    }

    #[test]
    fn expire_evicts_only_stale_peers() {
        let start = Instant::now();
        let ttl = Duration::from_secs(10);
        let mut table = PeerTable::new();
        let fresh = PeerId::from_bytes([1; PEER_ID_LEN]);
        let stale = PeerId::from_bytes([2; PEER_ID_LEN]);
        table.insert(
            PeerInfo::new(fresh, address(1)),
            start + Duration::from_secs(9),
        );
        table.insert(PeerInfo::new(stale, address(2)), start);

        let evicted = table.expire(start + Duration::from_secs(11), ttl);
        assert_eq!(evicted, vec![stale]);
        let remaining = table.snapshot();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, fresh);
    }
}
