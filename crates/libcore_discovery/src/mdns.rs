//! Tier 2 — mDNS/DNS-SD registration and browsing.
//!
//! AirBridge registers an `_airbridge._tcp.local.` service instance and
//! browses for the same service. Unlike a general-purpose resolver, this tier
//! owns its multicast socket directly:
//!
//! * it must bind port 5353 with `SO_REUSEADDR` and join `224.0.0.251`, which
//!   a caching resolver has no API for;
//! * it must answer queries for its own instance, which makes it a responder,
//!   not a client;
//! * mDNS is link-local and never forwarded, so a resolver cache would only
//!   add state that can go stale.
//!
//! hickory-proto is used for exactly what it is good at: the DNS wire format.
//! [`build_announcement`] and [`build_query`] are pure functions returning a
//! [`Message`], and [`PeerAssembler`] is a pure record-to-[`PeerInfo`] state
//! machine. That keeps the socket-free logic unit-testable, which matters
//! because multicast loopback is unreliable on Windows.
//!
//! # What is advertised
//!
//! ```text
//! PTR   _airbridge._tcp.local.        -> AirBridge-<id8>._airbridge._tcp.local.
//! SRV   AirBridge-<id8>...            -> airbridge-<id32>.local.:<listen_port>
//! TXT   AirBridge-<id8>...            -> id=<id32> v=1 [n=<display name>]
//! A     airbridge-<id32>.local.       -> <local IPv4 address>
//! ```
//!
//! The id lives in TXT rather than in the instance name so that the full
//! 128-bit value survives DNS escaping rules. A record without a parseable
//! `id=` is ignored: on our own service type that is a foreign or malformed
//! advertiser, not a peer.
//!
//! # Failure handling
//!
//! A socket that cannot be opened or a multicast group that cannot be joined
//! is fatal for the tier (and, under the fallback policy, escalates to the
//! subnet scan). A malformed datagram is not: it is ignored, with at most one
//! diagnostic per [`DECODE_ERROR_INTERVAL`]. The same applies to receive
//! errors, which on Windows routinely surface as `WSAECONNRESET` when an ICMP
//! port-unreachable comes back for an earlier multicast send; the socket is
//! still perfectly usable and the loop continues.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hickory_proto::op::{Message, OpCode, Query};
use hickory_proto::rr::rdata::{A, PTR, SRV, TXT};
use hickory_proto::rr::{Name, RData, Record, RecordType};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;

use crate::config::{MdnsConfig, MDNS_MULTICAST_ADDR, MDNS_PORT, MDNS_RECORD_TTL_SECS};
use crate::coordinator::Observation;
use crate::error::DiscoveryError;
use crate::event::{publish_error, publish_state, EventBus, TierId, TierState};
use crate::iface::primary_ipv4;
use crate::peer::{PeerId, PeerInfo};

/// Largest mDNS datagram we are willing to receive (RFC 6762 section 17).
const MAX_DATAGRAM: usize = 9000;

/// Version field carried in TXT; lets a future format change be detected.
const TXT_VERSION: u8 = 1;

/// Minimum spacing between answers to incoming queries, so that a query flood
/// cannot turn this node into a multicast amplifier.
const ANSWER_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Minimum spacing between malformed-datagram diagnostics.
const DECODE_ERROR_INTERVAL: Duration = Duration::from_secs(10);

/// Minimum spacing between receive-error diagnostics.
const RECV_ERROR_INTERVAL: Duration = Duration::from_secs(10);

/// How long a receive error pauses the loop, so a permanently broken socket
/// cannot spin a core.
const RECV_ERROR_PAUSE: Duration = Duration::from_millis(25);

/// Upper bound on assembler state, so a hostile link cannot grow it without
/// limit. Reaching it clears the caches; legitimate peers re-announce.
const MAX_ASSEMBLED: usize = 256;

/// Longest display name copied into a TXT string, in bytes.
const MAX_TXT_NAME: usize = 200;

/// The service instance this node announces.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ServiceAdvertisement<'a> {
    /// DNS-SD service type, e.g. `_airbridge._tcp.local.`.
    pub(crate) service_type: &'a str,

    /// The announcing peer.
    pub(crate) peer_id: PeerId,

    /// Display name, if the node has one.
    pub(crate) name: Option<&'a str>,

    /// Port peers should connect to.
    pub(crate) port: u16,

    /// Local IPv4 address placed in the A record.
    pub(crate) address: Ipv4Addr,
}

/// Normalizes a service type to a fully qualified [`Name`].
fn service_name(service_type: &str) -> Result<Name, DiscoveryError> {
    let qualified = if service_type.ends_with('.') {
        service_type.to_owned()
    } else {
        format!("{service_type}.")
    };
    Ok(Name::from_ascii(qualified)?)
}

/// Builds the unsolicited response that registers this node.
///
/// `ttl` is a full record lifetime for announcements and `0` for the goodbye
/// packet sent on shutdown (RFC 6762 section 10.1).
fn build_announcement(
    advertisement: &ServiceAdvertisement<'_>,
    ttl: u32,
) -> Result<Message, DiscoveryError> {
    let service = service_name(advertisement.service_type)?;
    let id = advertisement.peer_id.to_hex();
    let short = &id[..8];

    let instance = Name::from_ascii(format!("AirBridge-{short}.{}", service.to_ascii()))?;
    let target = Name::from_ascii(format!("airbridge-{id}.local."))?;

    let mut message = Message::response(0, OpCode::Query);
    message.metadata.authoritative = true;
    message.metadata.recursion_desired = false;

    // PTR is a shared record: no cache-flush bit, because several instances of
    // the same service legitimately coexist in one cache.
    message.add_answer(Record::from_rdata(
        service,
        ttl,
        RData::PTR(PTR(instance.clone())),
    ));

    // SRV, TXT and A are unique to this instance and flush stale values.
    message.add_answer(unique(Record::from_rdata(
        instance.clone(),
        ttl,
        RData::SRV(SRV::new(0, 0, advertisement.port, target.clone())),
    )));

    let mut txt = vec![format!("id={id}"), format!("v={TXT_VERSION}")];
    if let Some(name) = advertisement.name {
        let name = truncate_txt(name);
        if !name.is_empty() {
            txt.push(format!("n={name}"));
        }
    }
    message.add_answer(unique(Record::from_rdata(
        instance,
        ttl,
        RData::TXT(TXT::new(txt)),
    )));

    message.add_additional(unique(Record::from_rdata(
        target,
        ttl,
        RData::A(A(advertisement.address)),
    )));

    Ok(message)
}

/// Marks a record as unique to its owner.
fn unique(mut record: Record) -> Record {
    record.mdns_cache_flush = true;
    record
}

/// Builds a one-question PTR browse.
fn build_query(service_type: &str) -> Result<Message, DiscoveryError> {
    let service = service_name(service_type)?;
    let mut query = Query::query(service, RecordType::PTR);
    query.set_mdns_unicast_response(false);

    let mut message = Message::query();
    // RFC 6762 section 18.1: multicast DNS messages carry ID zero.
    message.metadata.id = 0;
    message.metadata.recursion_desired = false;
    message.add_query(query);
    Ok(message)
}

/// Encodes a message for the wire.
fn encode(message: &Message) -> Result<Vec<u8>, DiscoveryError> {
    Ok(message.to_vec()?)
}

/// Truncates a display name so it fits one DNS character-string.
fn truncate_txt(value: &str) -> String {
    if value.len() <= MAX_TXT_NAME {
        return value.to_owned();
    }
    let mut end = MAX_TXT_NAME;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// Partial DNS-SD state for one service instance.
#[derive(Clone, Debug, Default)]
struct InstanceState {
    target: Option<String>,
    port: Option<u16>,
    peer_id: Option<PeerId>,
    name: Option<String>,
}

/// Turns DNS-SD records into peers.
///
/// One announcement normally carries PTR, SRV, TXT and A together, but the
/// records may also be split across messages. The assembler therefore keeps
/// per-instance state and emits a peer as soon as the id (TXT), port (SRV) and
/// at least one address (A) are all known. Re-observing the same records is
/// idempotent; deduplication against the peer table happens in the
/// coordinator.
#[derive(Debug)]
pub(crate) struct PeerAssembler {
    service: Name,
    service_text: String,
    local_id: PeerId,
    instances: HashMap<String, InstanceState>,
    targets: HashMap<String, Vec<Ipv4Addr>>,
}

impl PeerAssembler {
    /// Creates an assembler for an already-parsed service name.
    fn from_name(service: Name, local_id: PeerId) -> Self {
        Self {
            service_text: service.to_ascii().to_lowercase(),
            service,
            local_id,
            instances: HashMap::new(),
            targets: HashMap::new(),
        }
    }

    /// Ingests one message and returns every peer it completed.
    pub(crate) fn observe(&mut self, message: &Message) -> Vec<PeerInfo> {
        for record in message.answers.iter().chain(message.additionals.iter()) {
            let key = record.name.to_ascii().to_lowercase();
            match &record.data {
                RData::PTR(ptr) => {
                    if record.name == self.service {
                        let instance = ptr.0.to_ascii().to_lowercase();
                        if instance.ends_with(&self.service_text) {
                            self.instance_entry(instance);
                        }
                    }
                }
                RData::SRV(srv) => {
                    if key.ends_with(&self.service_text) {
                        let state = self.instance_entry(key);
                        state.target = Some(srv.target.to_ascii().to_lowercase());
                        state.port = Some(srv.port);
                    }
                }
                RData::TXT(txt) => {
                    if key.ends_with(&self.service_text) {
                        let state = self.instance_entry(key);
                        for field in txt.txt_data.iter() {
                            let text = String::from_utf8_lossy(field);
                            if let Some(value) = text.strip_prefix("id=") {
                                if let Ok(id) = PeerId::from_hex(value.trim()) {
                                    state.peer_id = Some(id);
                                }
                            } else if let Some(value) = text.strip_prefix("n=") {
                                if !value.is_empty() {
                                    state.name = Some(value.to_owned());
                                }
                            }
                        }
                    }
                }
                RData::A(address) => {
                    let addresses = self.target_entry(key);
                    if !addresses.contains(&address.0) {
                        addresses.push(address.0);
                    }
                }
                _ => {}
            }
        }

        let mut peers = Vec::new();
        for (instance, state) in &self.instances {
            let Some(id) = state.peer_id else { continue };
            if id == self.local_id {
                continue;
            }
            let Some(port) = state.port else { continue };
            let Some(target) = state.target.as_ref() else {
                continue;
            };
            let Some(addresses) = self.targets.get(target) else {
                continue;
            };
            if addresses.is_empty() {
                continue;
            }
            let name = state.name.clone().or_else(|| instance_label(instance));
            peers.push(PeerInfo {
                id,
                name,
                addresses: addresses
                    .iter()
                    .map(|ip| SocketAddr::V4(SocketAddrV4::new(*ip, port)))
                    .collect(),
            });
        }
        peers
    }

    /// Returns the per-instance state, clearing both caches if a hostile link
    /// has pushed past [`MAX_ASSEMBLED`] distinct instances.
    fn instance_entry(&mut self, key: String) -> &mut InstanceState {
        if self.instances.len() >= MAX_ASSEMBLED && !self.instances.contains_key(&key) {
            self.instances.clear();
            self.targets.clear();
        }
        self.instances.entry(key).or_default()
    }

    /// Returns the address list of a target, bounded like the instance cache.
    fn target_entry(&mut self, key: String) -> &mut Vec<Ipv4Addr> {
        if self.targets.len() >= MAX_ASSEMBLED && !self.targets.contains_key(&key) {
            self.targets.clear();
        }
        self.targets.entry(key).or_default()
    }
}

/// Extracts the instance label (the part before the service suffix).
fn instance_label(instance: &str) -> Option<String> {
    instance
        .split('.')
        .next()
        .filter(|label| !label.is_empty())
        .map(str::to_owned)
}

/// Returns whether a message asks for our service type.
fn queries_service(message: &Message, service: &Name) -> bool {
    message.queries.iter().any(|query| {
        query.name() == service && matches!(query.query_type(), RecordType::PTR | RecordType::ANY)
    })
}

/// Creates the multicast socket and joins the mDNS group on `interface`.
fn open_socket(interface: Ipv4Addr) -> Result<UdpSocket, DiscoveryError> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, MDNS_PORT)).into())?;
    socket.join_multicast_v4(&MDNS_MULTICAST_ADDR, &interface)?;
    socket.set_multicast_if_v4(&interface)?;
    socket.set_multicast_ttl_v4(255)?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_nonblocking(true)?;
    let socket: std::net::UdpSocket = socket.into();
    Ok(UdpSocket::from_std(socket)?)
}

/// Reports a fatal setup failure for the tier and asks the coordinator to
/// consider the subnet fallback.
fn fail(bus: &EventBus, observations: &mpsc::UnboundedSender<Observation>, error: DiscoveryError) {
    publish_state(bus, TierId::Mdns, TierState::Failed(Arc::new(error)));
    let _ = observations.send(Observation::MdnsUnavailable);
}

/// Runs the tier until shutdown.
///
/// The task reports its own lifecycle and errors on `bus`, forwards completed
/// peers to `observations`, and sends a goodbye packet on the way out.
pub(crate) async fn run(
    config: MdnsConfig,
    peer_id: PeerId,
    peer_name: Option<String>,
    port: u16,
    bus: EventBus,
    observations: mpsc::UnboundedSender<Observation>,
    mut shutdown: watch::Receiver<bool>,
) {
    publish_state(&bus, TierId::Mdns, TierState::Starting);
    if *shutdown.borrow() {
        publish_state(&bus, TierId::Mdns, TierState::Stopped);
        return;
    }

    let interface = match config.interface {
        Some(interface) => interface,
        None => match primary_ipv4() {
            Ok(address) => address,
            Err(error) => {
                fail(&bus, &observations, error);
                return;
            }
        },
    };

    let socket = match open_socket(interface) {
        Ok(socket) => socket,
        Err(error) => {
            fail(&bus, &observations, error);
            return;
        }
    };

    let service = match service_name(&config.service_type) {
        Ok(service) => service,
        Err(error) => {
            fail(&bus, &observations, error);
            return;
        }
    };
    let mut assembler = PeerAssembler::from_name(service.clone(), peer_id);

    let advertisement = ServiceAdvertisement {
        service_type: &config.service_type,
        peer_id,
        name: peer_name.as_deref(),
        port,
        address: interface,
    };
    let announcement = match build_announcement(&advertisement, MDNS_RECORD_TTL_SECS)
        .and_then(|message| encode(&message))
    {
        Ok(bytes) => bytes,
        Err(error) => {
            fail(&bus, &observations, error);
            return;
        }
    };
    let goodbye = build_announcement(&advertisement, 0)
        .and_then(|message| encode(&message))
        .ok();
    let query = match build_query(&config.service_type).and_then(|message| encode(&message)) {
        Ok(bytes) => bytes,
        Err(error) => {
            fail(&bus, &observations, error);
            return;
        }
    };

    publish_state(&bus, TierId::Mdns, TierState::Active);

    let multicast = SocketAddr::V4(SocketAddrV4::new(MDNS_MULTICAST_ADDR, MDNS_PORT));
    let mut ticker = tokio::time::interval(config.query_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut buffer = vec![0_u8; MAX_DATAGRAM];
    let mut last_answer: Option<Instant> = None;
    let mut last_decode_error: Option<Instant> = None;
    let mut last_recv_error: Option<Instant> = None;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = ticker.tick() => {
                // Best effort: a temporarily unreachable link must not kill
                // the tier, and the next tick retries.
                let _ = socket.send_to(&announcement, multicast).await;
                let _ = socket.send_to(&query, multicast).await;
            }
            received = socket.recv_from(&mut buffer) => {
                match received {
                    Ok((len, from)) => {
                        let Ok(message) = Message::from_vec(&buffer[..len]) else {
                            if last_decode_error.is_none_or(|at| at.elapsed() >= DECODE_ERROR_INTERVAL) {
                                publish_error(
                                    &bus,
                                    TierId::Mdns,
                                    DiscoveryError::Encoding("malformed mdns message"),
                                );
                                last_decode_error = Some(Instant::now());
                            }
                            continue;
                        };

                        for peer in assembler.observe(&message) {
                            let _ = observations.send(Observation::Mdns(peer));
                        }

                        let may_answer = last_answer.is_none_or(|at| at.elapsed() >= ANSWER_MIN_INTERVAL);
                        if may_answer && queries_service(&message, &service) {
                            // Honour the unicast-response bit; otherwise stay
                            // on the multicast group.
                            let destination = if message
                                .queries
                                .iter()
                                .any(|query| query.mdns_unicast_response())
                            {
                                from
                            } else {
                                multicast
                            };
                            let _ = socket.send_to(&announcement, destination).await;
                            last_answer = Some(Instant::now());
                        }
                    }
                    Err(error) => {
                        if last_recv_error.is_none_or(|at| at.elapsed() >= RECV_ERROR_INTERVAL) {
                            publish_error(&bus, TierId::Mdns, DiscoveryError::Io(error));
                            last_recv_error = Some(Instant::now());
                        }
                        tokio::time::sleep(RECV_ERROR_PAUSE).await;
                    }
                }
            }
        }
    }

    if let Some(goodbye) = goodbye {
        let _ = socket.send_to(&goodbye, multicast).await;
    }
    publish_state(&bus, TierId::Mdns, TierState::Stopped);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use hickory_proto::op::MessageType;

    use super::*;

    const SERVICE: &str = crate::config::MDNS_SERVICE_TYPE;

    fn advertisement<'a>(
        name: Option<&'a str>,
        port: u16,
        address: Ipv4Addr,
    ) -> ServiceAdvertisement<'a> {
        ServiceAdvertisement {
            service_type: SERVICE,
            peer_id: PeerId::from_bytes([0x11; 16]),
            name,
            port,
            address,
        }
    }

    fn roundtrip(message: &Message) -> Message {
        Message::from_vec(&encode(message).unwrap()).unwrap()
    }

    fn assembler(local_id: PeerId) -> PeerAssembler {
        PeerAssembler::from_name(service_name(SERVICE).unwrap(), local_id)
    }

    #[test]
    fn announcement_is_assembled_into_a_peer() {
        let advertisement =
            advertisement(Some("Test Laptop"), 42517, Ipv4Addr::new(192, 168, 1, 42));
        let message = build_announcement(&advertisement, MDNS_RECORD_TTL_SECS).unwrap();
        assert_eq!(message.metadata.id, 0);
        assert_eq!(message.metadata.message_type, MessageType::Response);
        assert!(message.metadata.authoritative);

        let decoded = roundtrip(&message);
        let mut assembler = assembler(PeerId::from_bytes([0x22; 16]));
        let peers = assembler.observe(&decoded);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].id, advertisement.peer_id);
        assert_eq!(peers[0].name.as_deref(), Some("Test Laptop"));
        assert_eq!(
            peers[0].addresses,
            vec!["192.168.1.42:42517".parse().unwrap()]
        );

        // Re-observing the same records must not duplicate the peer.
        assert_eq!(assembler.observe(&decoded), peers);
    }

    #[test]
    fn announcement_falls_back_to_instance_label_for_the_name() {
        let advertisement = advertisement(None, 9, Ipv4Addr::new(10, 0, 0, 2));
        let message = build_announcement(&advertisement, MDNS_RECORD_TTL_SECS).unwrap();
        let mut assembler = assembler(PeerId::from_bytes([0x22; 16]));
        let peers = assembler.observe(&roundtrip(&message));
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name.as_deref(), Some("airbridge-11111111"));
    }

    #[test]
    fn local_announcements_are_never_returned_as_peers() {
        let advertisement = advertisement(Some("self"), 1, Ipv4Addr::new(10, 0, 0, 3));
        let message = build_announcement(&advertisement, MDNS_RECORD_TTL_SECS).unwrap();
        let mut assembler = assembler(advertisement.peer_id);
        assert!(assembler.observe(&roundtrip(&message)).is_empty());
    }

    #[test]
    fn records_split_across_messages_are_reassembled() {
        let advertisement = advertisement(Some("Split"), 7, Ipv4Addr::new(10, 0, 0, 4));
        let message = build_announcement(&advertisement, MDNS_RECORD_TTL_SECS).unwrap();

        let mut ptr = Message::response(0, OpCode::Query);
        ptr.add_answer(message.answers[0].clone());
        let mut srv_txt = Message::response(0, OpCode::Query);
        for record in &message.answers[1..] {
            srv_txt.add_answer(record.clone());
        }
        let mut add_a = Message::response(0, OpCode::Query);
        for record in &message.additionals {
            add_a.add_additional(record.clone());
        }

        let mut assembler = assembler(PeerId::from_bytes([0x23; 16]));
        assert!(assembler.observe(&ptr).is_empty());
        assert!(assembler.observe(&srv_txt).is_empty());
        let peers = assembler.observe(&add_a);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].addresses, vec!["10.0.0.4:7".parse().unwrap()]);
    }

    #[test]
    fn records_from_another_service_are_ignored() {
        let mut advertisement = advertisement(None, 1, Ipv4Addr::new(10, 0, 0, 5));
        advertisement.service_type = "_something-else._tcp.local.";
        let message = build_announcement(&advertisement, MDNS_RECORD_TTL_SECS).unwrap();
        let mut assembler = assembler(PeerId::from_bytes([0x24; 16]));
        assert!(assembler.observe(&roundtrip(&message)).is_empty());
    }

    #[test]
    fn unparseable_ids_do_not_produce_peers() {
        let advertisement = advertisement(None, 1, Ipv4Addr::new(10, 0, 0, 6));
        let mut message = build_announcement(&advertisement, MDNS_RECORD_TTL_SECS).unwrap();
        for record in &mut message.answers {
            if let RData::TXT(txt) = &mut record.data {
                *txt = TXT::new(vec!["id=not-hex".to_owned()]);
            }
        }
        let mut assembler = assembler(PeerId::from_bytes([0x25; 16]));
        assert!(assembler.observe(&roundtrip(&message)).is_empty());
    }

    #[test]
    fn query_has_the_mdns_shape() {
        let message = build_query(SERVICE).unwrap();
        assert_eq!(message.metadata.id, 0);
        assert_eq!(message.metadata.message_type, MessageType::Query);
        assert!(!message.metadata.recursion_desired);
        assert_eq!(message.queries.len(), 1);
        assert_eq!(message.queries[0].query_type(), RecordType::PTR);
        assert_eq!(message.queries[0].name(), &service_name(SERVICE).unwrap());
        assert!(!message.queries[0].mdns_unicast_response());

        let decoded = roundtrip(&message);
        assert!(queries_service(&decoded, &service_name(SERVICE).unwrap()));
        assert!(!queries_service(
            &decoded,
            &service_name("_other._tcp.local.").unwrap()
        ));
    }

    #[test]
    fn goodbye_records_expire_immediately() {
        let advertisement = advertisement(None, 1, Ipv4Addr::new(10, 0, 0, 7));
        let message = build_announcement(&advertisement, 0).unwrap();
        assert!(message.answers.iter().all(|record| record.ttl == 0));
        assert!(message.additionals.iter().all(|record| record.ttl == 0));
    }

    #[test]
    fn long_names_are_truncated_on_a_char_boundary() {
        let name = "é".repeat(150);
        assert!(truncate_txt(&name).len() <= MAX_TXT_NAME);
        assert!(truncate_txt(&name).is_char_boundary(truncate_txt(&name).len()));
        assert_eq!(truncate_txt("short"), "short");
    }
}
