//! Tier 3 — active IPv4 subnet probing.
//!
//! When the passive tiers cannot find anyone (or are configured to run
//! alongside them), the coordinator sweeps the local `/24` for hosts that
//! answer a plain HTTP probe:
//!
//! ```text
//! GET http://<host>:<port>/api/v1/register
//! ```
//!
//! A host that is not AirBridge refuses, times out or answers with a non-2xx
//! status; that is the overwhelmingly common case and is not an error. A
//! host that *is* AirBridge answers 2xx and identifies itself with headers:
//!
//! ```text
//! x-airbridge-peer-id: <32 hex digits>
//! x-airbridge-name:    <UTF-8 display name>      (optional)
//! x-airbridge-port:    <port serving transfers>  (optional)
//! ```
//!
//! The contract is deliberately header-only: no JSON parser, no body read, and
//! no `serde` in the dependency graph. A peer that omits the id gets an id
//! derived from its address, so the response still produces a usable peer.
//!
//! # Timing
//!
//! Every probe is bounded by [`crate::config::SUBNET_PROBE_TIMEOUT`] (1.5 s by
//! default) on the whole request, not per read. Probes run concurrently with a
//! semaphore cap (128 by default), one Tokio task per host. On a `/24` this
//! finishes in a couple of seconds even when most hosts are silent, and a
//! firewalled host is indistinguishable from an absent one — which is the
//! honest outcome, not an error to report per address.
//!
//! # Shutdown
//!
//! The sweep holds a shutdown watch receiver. When it fires, queued probes are
//! dropped and in-flight tasks are aborted; a scan never delays shutdown by its
//! worst-case timeout.

use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;

use reqwest::header::HeaderMap;
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::task::JoinSet;

use crate::config::{
    Ipv4Subnet, SubnetScanConfig, PEER_ID_HEADER, PEER_NAME_HEADER, PEER_PORT_HEADER,
    PROBE_USER_AGENT, REGISTER_PATH,
};
use crate::coordinator::Observation;
use crate::error::DiscoveryError;
use crate::event::{publish, publish_error, DiscoveryEvent, EventBus, TierId};
use crate::iface::primary_ipv4;
use crate::peer::{PeerId, PeerInfo};

/// Sweeps the configured (or interface-derived) networks once.
///
/// Returns when every probe has finished, the scan failed to start, or
/// shutdown was requested. Progress is reported on `bus` as
/// [`DiscoveryEvent::SubnetScanStarted`] and
/// [`DiscoveryEvent::SubnetScanFinished`].
pub(crate) async fn run_scan(
    config: SubnetScanConfig,
    local_id: PeerId,
    bus: EventBus,
    observations: mpsc::UnboundedSender<Observation>,
    mut shutdown: watch::Receiver<bool>,
) {
    if *shutdown.borrow() {
        return;
    }

    let subnets = match resolve_subnets(&config) {
        Ok(subnets) => subnets,
        Err(error) => {
            publish_error(&bus, TierId::Subnet, error);
            return;
        }
    };
    publish(
        &bus,
        DiscoveryEvent::SubnetScanStarted {
            subnets: subnets.len(),
        },
    );

    let client = match reqwest::Client::builder()
        .timeout(config.timeout)
        .connect_timeout(config.timeout)
        .no_proxy()
        .user_agent(PROBE_USER_AGENT)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            publish_error(&bus, TierId::Subnet, DiscoveryError::Http(error));
            return;
        }
    };

    let semaphore = Arc::new(Semaphore::new(config.concurrency));
    let mut tasks = JoinSet::new();
    let mut probed = 0_usize;

    'hosts: for subnet in &subnets {
        for host in subnet.hosts() {
            if *shutdown.borrow() {
                break 'hosts;
            }
            let permit = match Arc::clone(&semaphore).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break 'hosts,
            };
            let client = client.clone();
            let observations = observations.clone();
            let port = config.port;
            tasks.spawn(async move {
                let _permit = permit;
                if let Some(peer) =
                    probe(&client, SocketAddrV4::new(host, port), local_id, port).await
                {
                    let _ = observations.send(Observation::Subnet(peer));
                    true
                } else {
                    false
                }
            });
            probed += 1;
        }
    }

    let mut responded = 0_usize;
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    break;
                }
            }
            joined = tasks.join_next() => {
                match joined {
                    Some(Ok(true)) => responded += 1,
                    Some(Ok(false)) | Some(Err(_)) => {}
                    None => break,
                }
            }
        }
    }

    publish(
        &bus,
        DiscoveryEvent::SubnetScanFinished {
            probed,
            responded,
            discovered: responded,
        },
    );
}

/// Returns the networks to sweep: the configured list, or the `/24` of the
/// primary interface when none were given.
fn resolve_subnets(config: &SubnetScanConfig) -> Result<Vec<Ipv4Subnet>, DiscoveryError> {
    if !config.subnets.is_empty() {
        return Ok(config.subnets.clone());
    }
    let address = primary_ipv4()?;
    Ok(vec![Ipv4Subnet::from_host(address)])
}

/// Probes one host and converts a 2xx answer into a peer.
///
/// `fallback_port` is used when the peer does not advertise a transfer port;
/// it is the port the probe just succeeded on, the only port known to be open.
async fn probe(
    client: &reqwest::Client,
    address: SocketAddrV4,
    local_id: PeerId,
    fallback_port: u16,
) -> Option<PeerInfo> {
    let url = format!("http://{address}{REGISTER_PATH}");
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    let headers = response.headers();
    let id = header_str(headers, PEER_ID_HEADER)
        .and_then(|text| PeerId::from_hex(text).ok())
        .unwrap_or_else(|| PeerId::derive_from(&SocketAddr::V4(address)));
    if id == local_id {
        return None;
    }

    let name = header_str(headers, PEER_NAME_HEADER).map(str::to_owned);
    let port = header_str(headers, PEER_PORT_HEADER)
        .and_then(|text| text.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .unwrap_or(fallback_port);

    Some(PeerInfo {
        id,
        name,
        addresses: vec![SocketAddr::V4(SocketAddrV4::new(*address.ip(), port))],
    })
}

/// Reads a trimmed, non-empty ASCII header value.
fn header_str<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn explicit_subnets_win_over_the_interface() {
        let subnet = Ipv4Subnet::new("10.9.0.0".parse().unwrap(), 24).unwrap();
        let config = SubnetScanConfig::new().with_subnets(vec![subnet]);
        assert_eq!(resolve_subnets(&config).unwrap(), vec![subnet]);
    }

    #[test]
    fn header_lookup_trims_and_rejects_empty_values() {
        let mut headers = HeaderMap::new();
        headers.insert(PEER_NAME_HEADER, "  Laptop  ".parse().unwrap());
        assert_eq!(header_str(&headers, PEER_NAME_HEADER), Some("Laptop"));
        headers.insert(PEER_PORT_HEADER, "   ".parse().unwrap());
        assert_eq!(header_str(&headers, PEER_PORT_HEADER), None);
        assert_eq!(header_str(&headers, PEER_ID_HEADER), None);
    }
}
