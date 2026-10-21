//! End-to-end coordinator tests.
//!
//! These exercise the seams that unit tests cannot: the BLE lifecycle through
//! a mock backend, the event stream, deduplication, and the tier-3 fallback
//! against a real HTTP listener on loopback. mDNS is covered by pure-function
//! tests inside the crate because multicast loopback is not reliable in a test
//! environment.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use libcore_discovery::{
    BleObservation, DiscoveryConfig, DiscoveryCoordinator, DiscoveryEvent, Ipv4Subnet, MdnsConfig,
    MockBleBackend, PeerId, PeerInfo, SubnetPolicy, SubnetScanConfig, TierId,
    DEFAULT_DISCOVERY_PORT,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::broadcast::Receiver;
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// Waits for the first event matching `predicate`, panicking on timeout.
async fn wait_for<F>(
    events: &mut Receiver<DiscoveryEvent>,
    duration: Duration,
    mut predicate: F,
) -> DiscoveryEvent
where
    F: FnMut(&DiscoveryEvent) -> bool,
{
    timeout(duration, async {
        loop {
            match events.recv().await {
                Ok(event) if predicate(&event) => return event,
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => panic!("event channel closed before the expected event"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for a discovery event"))
}

/// Waits until `condition` is true, polling every 10 ms.
async fn eventually(condition: impl Fn() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition did not become true within two seconds");
}

/// A hand-rolled HTTP peer: answers every request with the AirBridge headers.
struct TestPeer {
    port: u16,
    task: JoinHandle<()>,
}

impl TestPeer {
    async fn spawn(id: PeerId, name: &'static str, transfer_port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let id = id.to_hex();
                let name = name.to_owned();
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 1024];
                    let _ = stream.read(&mut buffer).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\n\
                         content-length: 0\r\n\
                         connection: close\r\n\
                         x-airbridge-peer-id: {id}\r\n\
                         x-airbridge-name: {name}\r\n\
                         x-airbridge-port: {transfer_port}\r\n\
                         \r\n"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { port, task }
    }
}

impl Drop for TestPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn ble_observations_are_deduplicated_and_merged() {
    let mock = Arc::new(MockBleBackend::new());
    let backend: Arc<dyn libcore_discovery::BleBackend> = mock.clone();
    let observer_id = PeerId::generate().unwrap();
    let config = DiscoveryConfig::new(PeerId::generate().unwrap(), DEFAULT_DISCOVERY_PORT)
        .with_ble(true)
        .with_mdns_enabled(false)
        .with_subnet_policy(SubnetPolicy::Disabled);
    let handle = DiscoveryCoordinator::start(config, Some(backend)).unwrap();
    let mut events = handle.events();

    eventually(|| mock.is_running()).await;

    let observation = BleObservation {
        peer_id: observer_id,
        port: 42517,
    };
    assert!(mock.emit(observation));

    let found = wait_for(&mut events, Duration::from_secs(2), |event| {
        matches!(event, DiscoveryEvent::PeerFound(_))
    })
    .await;
    match found {
        DiscoveryEvent::PeerFound(peer) => {
            assert_eq!(peer.id, observer_id);
            // BLE carries no IP address; the peer is still real.
            assert!(peer.addresses.is_empty());
        }
        other => panic!("expected PeerFound, got {other:?}"),
    }

    // A second sighting of the same id refreshes the entry without emitting a
    // second PeerFound.
    assert!(mock.emit(observation));
    assert!(timeout(Duration::from_millis(250), events.recv())
        .await
        .is_err());

    let peers = handle.peers();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].id, observer_id);

    handle.shutdown().await;
    assert!(!mock.is_running());
}

#[tokio::test]
async fn fallback_subnet_scan_finds_an_http_peer() {
    let peer_id = PeerId::generate().unwrap();
    let server = TestPeer::spawn(peer_id, "Test Peer", 49999).await;
    let subnet = Ipv4Subnet::new(Ipv4Addr::new(127, 0, 0, 0), 28).unwrap();
    let scan = SubnetScanConfig::new()
        .with_subnets(vec![subnet])
        .with_port(server.port)
        .with_concurrency(16)
        .with_timeout(Duration::from_millis(500));
    let config = DiscoveryConfig::new(PeerId::generate().unwrap(), DEFAULT_DISCOVERY_PORT)
        .with_ble(false)
        .with_mdns_enabled(false)
        .with_subnet_policy(SubnetPolicy::Fallback)
        .with_mdns_grace(Duration::from_millis(50))
        .with_subnet_scan(scan);
    let handle = DiscoveryCoordinator::start(config, None).unwrap();
    let mut events = handle.events();

    let mut found: Option<PeerInfo> = None;
    let mut saw_started = false;
    let mut saw_finished = false;
    timeout(Duration::from_secs(10), async {
        while found.is_none() || !saw_finished {
            match events.recv().await {
                Ok(DiscoveryEvent::SubnetScanStarted { subnets }) => {
                    assert_eq!(subnets, 1);
                    saw_started = true;
                }
                Ok(DiscoveryEvent::SubnetScanFinished {
                    probed,
                    responded,
                    discovered,
                }) => {
                    assert!(probed >= 14, "the /28 should be fully probed");
                    assert_eq!(responded, 1);
                    assert_eq!(discovered, 1);
                    saw_finished = true;
                }
                Ok(DiscoveryEvent::PeerFound(peer)) => found = Some(peer),
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => panic!("event channel closed during the scan"),
            }
        }
    })
    .await
    .expect("the fallback scan should finish");

    assert!(saw_started);
    let peer = found.unwrap();
    assert_eq!(peer.id, peer_id);
    assert_eq!(peer.name.as_deref(), Some("Test Peer"));
    assert_eq!(
        peer.addresses,
        vec![SocketAddr::from(([127, 0, 0, 1], 49999))]
    );
    assert_eq!(handle.peers().len(), 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn a_failed_mdns_tier_escalates_immediately() {
    let peer_id = PeerId::generate().unwrap();
    let server = TestPeer::spawn(peer_id, "Fallback", 42518).await;
    let subnet = Ipv4Subnet::new(Ipv4Addr::new(127, 0, 0, 0), 30).unwrap();
    let scan = SubnetScanConfig::new()
        .with_subnets(vec![subnet])
        .with_port(server.port)
        .with_concurrency(8)
        .with_timeout(Duration::from_millis(500));
    // A TEST-NET-3 address is not a local interface, so joining the multicast
    // group on it must fail and the tier must report itself as failed.
    let mdns = MdnsConfig::new().with_interface(Ipv4Addr::new(203, 0, 113, 7));
    let config = DiscoveryConfig::new(PeerId::generate().unwrap(), DEFAULT_DISCOVERY_PORT)
        .with_ble(false)
        .with_mdns(mdns)
        .with_subnet_policy(SubnetPolicy::Fallback)
        // Long enough that the scan can only have been triggered by the
        // failure, not by the grace timer.
        .with_mdns_grace(Duration::from_secs(30))
        .with_subnet_scan(scan);
    let handle = DiscoveryCoordinator::start(config, None).unwrap();
    let mut events = handle.events();

    let mut saw_failure = false;
    let found = timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Ok(DiscoveryEvent::TierStateChanged {
                    tier: TierId::Mdns,
                    state,
                }) if state.is_failed() => saw_failure = true,
                Ok(DiscoveryEvent::PeerFound(peer)) => return peer,
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => panic!("event channel closed during the fallback"),
            }
        }
    })
    .await
    .expect("a failed mdns tier should trigger a scan");

    assert!(saw_failure, "the mdns failure must be reported");
    assert_eq!(found.id, peer_id);
    assert_eq!(
        found.addresses,
        vec![SocketAddr::from(([127, 0, 0, 1], 42518))]
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn disabled_subnet_policy_never_probes() {
    let server = TestPeer::spawn(PeerId::generate().unwrap(), "Should Not Be Found", 42519).await;
    let subnet = Ipv4Subnet::new(Ipv4Addr::new(127, 0, 0, 0), 28).unwrap();
    let scan = SubnetScanConfig::new()
        .with_subnets(vec![subnet])
        .with_port(server.port)
        .with_timeout(Duration::from_millis(200));
    let config = DiscoveryConfig::new(PeerId::generate().unwrap(), DEFAULT_DISCOVERY_PORT)
        .with_ble(false)
        .with_mdns_enabled(false)
        .with_subnet_policy(SubnetPolicy::Disabled)
        .with_mdns_grace(Duration::from_millis(10))
        .with_subnet_scan(scan);
    let handle = DiscoveryCoordinator::start(config, None).unwrap();
    let mut events = handle.events();

    assert!(!handle.trigger_subnet_scan());
    tokio::time::sleep(Duration::from_millis(300)).await;

    loop {
        match events.try_recv() {
            Ok(DiscoveryEvent::SubnetScanStarted { .. })
            | Ok(DiscoveryEvent::SubnetScanFinished { .. })
            | Ok(DiscoveryEvent::PeerFound(_)) => panic!("a disabled policy must not probe"),
            Ok(_) => {}
            Err(TryRecvError::Lagged(_)) => continue,
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
    assert!(handle.peers().is_empty());

    handle.shutdown().await;
}

#[tokio::test]
async fn a_manual_trigger_bypasses_the_grace_timer() {
    let peer_id = PeerId::generate().unwrap();
    let server = TestPeer::spawn(peer_id, "Manual", 42520).await;
    let subnet = Ipv4Subnet::new(Ipv4Addr::new(127, 0, 0, 0), 30).unwrap();
    let scan = SubnetScanConfig::new()
        .with_subnets(vec![subnet])
        .with_port(server.port)
        .with_timeout(Duration::from_millis(500));
    let config = DiscoveryConfig::new(PeerId::generate().unwrap(), DEFAULT_DISCOVERY_PORT)
        .with_ble(false)
        .with_mdns_enabled(false)
        .with_subnet_policy(SubnetPolicy::Fallback)
        // Far beyond the test duration: only the manual trigger can scan.
        .with_mdns_grace(Duration::from_secs(30))
        .with_subnet_scan(scan);
    let handle = DiscoveryCoordinator::start(config, None).unwrap();
    let mut events = handle.events();

    assert!(handle.trigger_subnet_scan());
    let found = wait_for(&mut events, Duration::from_secs(5), |event| {
        matches!(event, DiscoveryEvent::PeerFound(_))
    })
    .await;
    match found {
        DiscoveryEvent::PeerFound(peer) => assert_eq!(peer.id, peer_id),
        other => panic!("expected PeerFound, got {other:?}"),
    }

    handle.shutdown().await;
}
