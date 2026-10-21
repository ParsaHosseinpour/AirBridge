//! Error type for the discovery layer.

use core::fmt;

/// Errors produced while discovering or advertising peers.
///
/// Tier failures are not fatal to the coordinator: they are reported as
/// [`crate::DiscoveryEvent::TierStateChanged`] and, depending on policy,
/// trigger the subnet fallback. Only configuration errors stop a scan before
/// it starts.
#[derive(Debug)]
#[non_exhaustive]
pub enum DiscoveryError {
    /// A socket, file or task-level I/O operation failed.
    Io(std::io::Error),

    /// The DNS message codec rejected or could not build a message.
    Dns(hickory_proto::ProtoError),

    /// An HTTP probe failed in transit (connection refused, timeout, TLS).
    ///
    /// A probe that merely finds no listener is not an error: it is a host
    /// without a peer, which is the expected case for most of a `/24`.
    Http(reqwest::Error),

    /// The configuration cannot produce a working tier.
    InvalidConfig(&'static str),

    /// A platform BLE backend refused to start or stop.
    Backend(&'static str),

    /// A beacon or message carried bytes that do not decode.
    Encoding(&'static str),

    /// No interface address is available for the operation that needs one.
    NoUsableInterface,
}

impl From<std::io::Error> for DiscoveryError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<hickory_proto::ProtoError> for DiscoveryError {
    fn from(error: hickory_proto::ProtoError) -> Self {
        Self::Dns(error)
    }
}

impl From<reqwest::Error> for DiscoveryError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "i/o error: {error}"),
            Self::Dns(error) => write!(f, "dns error: {error}"),
            Self::Http(error) => write!(f, "http probe error: {error}"),
            Self::InvalidConfig(detail) => write!(f, "invalid discovery configuration: {detail}"),
            Self::Backend(detail) => write!(f, "ble backend failure: {detail}"),
            Self::Encoding(detail) => write!(f, "malformed discovery payload: {detail}"),
            Self::NoUsableInterface => f.write_str("no usable interface address"),
        }
    }
}

impl std::error::Error for DiscoveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Dns(error) => Some(error),
            Self::Http(error) => Some(error),
            _ => None,
        }
    }
}
