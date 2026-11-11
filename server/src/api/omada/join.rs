//! Join Omada clients to PDM's PVE remotes.
//!
//! The match map is keyed by everything that could plausibly be the IP or
//! host name of an Omada client: each remote node's literal host name, every
//! DNS A record for that host, and the cluster node name (when known from the
//! remote mapping cache). First matching remote wins.

use std::collections::HashMap;
use std::net::ToSocketAddrs;

use pdm_api_types::omada::{OmadaClientEntry, PveMatch};
use pdm_api_types::remotes::{Remote, RemoteType};
use proxmox_section_config::typed::SectionConfigData;

use crate::remote_cache::RemoteMappingCache;

/// One node of a PVE remote, as registered in PDM's remote config.
#[derive(Clone, Debug, Default)]
pub struct RemoteNodeEntry {
    pub hostname: String,
    /// Cluster node name, if the remote mapping cache knows it.
    pub node_name: Option<String>,
}

/// A PVE remote and the nodes it was configured with.
#[derive(Clone, Debug, Default)]
pub struct RemoteJoinEntry {
    pub id: String,
    pub nodes: Vec<RemoteNodeEntry>,
}

/// Build the match map: key → `(remote_id, node_name)`, first match wins.
pub fn build_match_map(
    remotes: &[RemoteJoinEntry],
    dns: &HashMap<String, Vec<String>>,
) -> HashMap<String, (String, Option<String>)> {
    let mut map: HashMap<String, (String, Option<String>)> = HashMap::new();

    for remote in remotes {
        for node in &remote.nodes {
            let value = (remote.id.clone(), node.node_name.clone());
            map.entry(node.hostname.clone())
                .or_insert_with(|| value.clone());
            for ip in dns.get(&node.hostname).into_iter().flatten() {
                map.entry(ip.clone()).or_insert_with(|| value.clone());
            }
            if let Some(node_name) = &node.node_name {
                map.entry(node_name.clone())
                    .or_insert_with(|| value.clone());
            }
        }
    }

    map
}

/// Annotate a single client entry in place.
///
/// The client IP is tried first; if it has no IP, the host name (lowercased
/// and trimmed) is used.
pub fn annotate_client(
    entry: &mut OmadaClientEntry,
    map: &HashMap<String, (String, Option<String>)>,
) {
    let matched = entry
        .client
        .ip
        .as_deref()
        .and_then(|ip| map.get(ip).cloned())
        .or_else(|| {
            entry
                .client
                .hostname
                .as_deref()
                .map(|host| host.trim().to_ascii_lowercase())
                .and_then(|host| map.get(&host).cloned())
        });

    if let Some((remote, node)) = matched {
        entry.pve = Some(PveMatch { remote, node });
    }
}

/// Collect one [`RemoteJoinEntry`] per PVE remote, annotating node names from
/// the remote mapping cache where known.
pub fn remote_join_entries(remotes: &SectionConfigData<Remote>) -> Vec<RemoteJoinEntry> {
    let mapping = RemoteMappingCache::get();

    remotes
        .values()
        .filter(|remote| remote.ty == RemoteType::Pve)
        .map(|remote| {
            let nodes = remote
                .nodes
                .iter()
                .map(|node| {
                    let node_name = mapping
                        .info_by_hostname(&remote.id, &node.hostname)
                        .and_then(|host| host.node_name().map(str::to_string));

                    RemoteNodeEntry {
                        hostname: node.hostname.clone(),
                        node_name,
                    }
                })
                .collect();

            RemoteJoinEntry {
                id: remote.id.clone(),
                nodes,
            }
        })
        .collect()
}

/// Resolve `host` to its IPv4 A records using the std resolver.
///
/// IP literals and ports are handled here; per-host errors are swallowed by
/// the caller.
pub fn std_resolver(host: &str) -> Result<Vec<String>, anyhow::Error> {
    // Strip a port if present ("host:port" — good enough for our purposes).
    let host = host.rsplit_once(':').map_or(host, |(host, _port)| host);

    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(Vec::new());
    }

    let mut ips = Vec::new();
    for addr in (host, 443u16).to_socket_addrs()? {
        if let std::net::IpAddr::V4(ip) = addr.ip() {
            ips.push(ip.to_string());
        }
    }

    Ok(ips)
}

/// Build the DNS map for `hosts`, resolving each entry with `resolver`.
///
/// Per-host resolution errors are swallowed (a dead DNS must not break the
/// whole endpoint); a failed host simply contributes no records.
pub fn build_dns_map(
    hosts: &[String],
    resolver: impl Fn(&str) -> Result<Vec<String>, anyhow::Error>,
) -> HashMap<String, Vec<String>> {
    hosts
        .iter()
        .map(|host| {
            let ips = resolver(host).unwrap_or_default();
            (host.clone(), ips)
        })
        .filter(|(_, ips)| !ips.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remotes() -> Vec<RemoteJoinEntry> {
        vec![RemoteJoinEntry {
            id: "pve-lab".into(),
            nodes: vec![
                RemoteNodeEntry {
                    hostname: "172.31.12.97".into(),
                    node_name: Some("pve".into()),
                },
                RemoteNodeEntry {
                    hostname: "node2.example".into(),
                    node_name: None,
                },
            ],
        }]
    }

    fn dns() -> HashMap<String, Vec<String>> {
        [("node2.example".into(), vec!["172.31.12.99".into()])].into()
    }

    #[test]
    fn matches_ip_literal_host_and_node_name() {
        let map = build_match_map(&remotes(), &dns());
        assert_eq!(
            map.get("172.31.12.97").map(|(r, _)| r.clone()),
            Some("pve-lab".into())
        );
        assert_eq!(
            map.get("pve").map(|(r, n)| (r.clone(), n.clone())),
            Some(("pve-lab".into(), Some("pve".into())))
        );
        assert_eq!(
            map.get("172.31.12.99").map(|(r, _)| r.clone()),
            Some("pve-lab".into())
        );
    }

    #[test]
    fn first_remote_wins_on_duplicate_keys() {
        let remotes = vec![
            RemoteJoinEntry {
                id: "first".into(),
                nodes: vec![RemoteNodeEntry {
                    hostname: "172.31.12.97".into(),
                    node_name: None,
                }],
            },
            RemoteJoinEntry {
                id: "second".into(),
                nodes: vec![RemoteNodeEntry {
                    hostname: "172.31.12.97".into(),
                    node_name: None,
                }],
            },
        ];
        let map = build_match_map(&remotes, &HashMap::new());
        assert_eq!(
            map.get("172.31.12.97").map(|(r, _)| r.clone()),
            Some("first".into())
        );
    }

    #[test]
    fn annotate_prefers_ip_over_hostname() {
        let map = build_match_map(&remotes(), &dns());

        // IP matches, host name does not.
        let mut entry: OmadaClientEntry =
            serde_json::from_str(r#"{"ip":"172.31.12.97","hostName":"something-else"}"#).unwrap();
        annotate_client(&mut entry, &map);
        assert_eq!(entry.pve.as_ref().unwrap().remote, "pve-lab");
        assert_eq!(entry.pve.as_ref().unwrap().node.as_deref(), Some("pve"));

        // No IP, host name matches a node name (case-insensitive).
        let mut entry: OmadaClientEntry = serde_json::from_str(r#"{"hostName":"  PVE "}"#).unwrap();
        annotate_client(&mut entry, &map);
        assert_eq!(entry.pve.as_ref().unwrap().remote, "pve-lab");

        // Nothing matches.
        let mut entry: OmadaClientEntry =
            serde_json::from_str(r#"{"ip":"10.0.0.5","hostName":"nope"}"#).unwrap();
        annotate_client(&mut entry, &map);
        assert!(entry.pve.is_none());
    }

    #[test]
    fn dns_failure_is_swallowed() {
        let hosts = vec!["good.example".into(), "bad.example".into()];
        let map = build_dns_map(&hosts, |host| {
            if host == "good.example" {
                Ok(vec!["172.31.12.99".into()])
            } else {
                Err(anyhow::anyhow!("dns down"))
            }
        });
        assert_eq!(map.get("good.example"), Some(&vec!["172.31.12.99".into()]));
        assert_eq!(map.get("bad.example"), None);
    }
}
