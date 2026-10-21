//! Best-effort discovery of the local IPv4 address.
//!
//! Enumerating interfaces portably needs a platform dependency
//! (`getifaddrs`, `GetAdaptersAddresses`); AirBridge only needs *one* routable
//! address per operation, so it uses the route table instead: connect a
//! datagram socket to a public address and ask the kernel which local address
//! it picked. A UDP `connect` performs no handshake and sends no packet, so
//! the call is offline-safe and has no side effects beyond the socket itself.
//!
//! Multi-homed hosts that need to advertise or scan a specific link can pin an
//! interface in [`crate::MdnsConfig`] or list networks explicitly in
//! [`crate::SubnetScanConfig`].

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

use crate::error::DiscoveryError;

/// Discovers the IPv4 address the routing table would use for off-link
/// traffic.
///
/// # Errors
///
/// Returns [`DiscoveryError::NoUsableInterface`] when the route lookup fails,
/// yields the unspecified address, or only loopback is available. Returns
/// [`DiscoveryError::Io`] when a socket cannot be created.
pub(crate) fn primary_ipv4() -> Result<Ipv4Addr, DiscoveryError> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    // TEST-NET-1 (RFC 5737). Using a documentation address makes it obvious
    // this is a route lookup, not a reachability check; no packet is sent.
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
    match socket.local_addr()? {
        SocketAddr::V4(address) if usable(*address.ip()) => Ok(*address.ip()),
        _ => Err(DiscoveryError::NoUsableInterface),
    }
}

/// Returns whether an address can carry link-local discovery.
fn usable(address: Ipv4Addr) -> bool {
    !address.is_unspecified() && !address.is_loopback() && !address.is_broadcast()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn unusable_addresses_are_rejected() {
        assert!(!usable(Ipv4Addr::UNSPECIFIED));
        assert!(!usable(Ipv4Addr::LOCALHOST));
        assert!(!usable(Ipv4Addr::BROADCAST));
        assert!(usable(Ipv4Addr::new(192, 168, 1, 10)));
    }
}
