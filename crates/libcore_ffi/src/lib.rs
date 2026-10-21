//! AirBridge foreign-function interface.
//!
//! Peer discovery and resumable file transfer, exposed to Kotlin and C#
//! through UniFFI 0.31. The crate-level documentation is maintained in
//! `README.md` and included here verbatim so rustdoc and the repository show
//! the same text.
#![doc = include_str!("../README.md")]

uniffi::setup_scaffolding!();

mod ble;
mod cancel;
mod discovery;
mod error;
mod runtime;
mod transfer;

pub use ble::{BleBackend, BleObservationSink, BleRequest};
pub use discovery::{
    DiscoveryConfig, DiscoveryEvent, DiscoveryTier, PeerDiscoveryEngine, PeerInfo, SubnetPolicy,
    TierState,
};
pub use error::{AbortReason, FfiError};
pub use transfer::{
    FileTransferSession, ReceiveOptions, SendOptions, TransferDirection, TransferEvent,
    TransferReport, TransferStatus,
};
