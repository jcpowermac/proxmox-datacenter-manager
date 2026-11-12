//! Join PVE guest NIC MACs with Omada switch ports.
//!
//! The Guests panel's data source (`/resources/list`) carries no MAC, so the
//! MACs are read from each guest's PVE config (works for running and stopped
//! guests alike) and matched against the Omada client list. NIC MACs are
//! stable, so they are memoized for a longer TTL than the upstream Omada data.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Error;
use pdm_api_types::omada::{
    OmadaClient, OmadaDevice, OmadaGuestPort, OmadaGuestPortMatch,
};
use pdm_api_types::remotes::{Remote, RemoteType};
use pdm_api_types::PRIV_SYS_AUDIT;
use proxmox_router::{Permission, RpcEnvironment};
use proxmox_schema::api;
use tokio::sync::Mutex as AsyncMutex;

use super::{omada_client, upstream_site};
use crate::context::PdmApplication;

/// How often to re-read guest NIC MACs from PVE (they rarely change).
const MAC_TTL: Duration = Duration::from_secs(60);

/// Long-lived memo for guest NIC MACs, separate from the 10s Omada memo.
static MAC_MEMO: OnceLock<AsyncMutex<HashMap<String, (Instant, serde_json::Value)>>> =
    OnceLock::new();

fn mac_memo() -> &'static AsyncMutex<HashMap<String, (Instant, serde_json::Value)>> {
    MAC_MEMO.get_or_init(Default::default)
}

/// Memoize `f` under `key` for [`MAC_TTL`]; on any error return an empty list
/// (a guest we can't read must not fail the whole endpoint).
async fn macs_get(
    key: &str,
    f: impl Future<Output = Result<Vec<String>, Error>>,
) -> Vec<String> {
    {
        let entries = mac_memo().lock().await;
        if let Some((at, value)) = entries.get(key) {
            if at.elapsed() < MAC_TTL {
                return serde_json::from_value(value.clone()).unwrap_or_default();
            }
        }
    }
    match f.await {
        Ok(macs) => {
            let serialized = serde_json::to_value(&macs).unwrap_or_default();
            mac_memo()
                .lock()
                .await
                .insert(key.to_string(), (Instant::now(), serialized.clone()));
            serde_json::from_value(serialized).unwrap_or_default()
        }
        Err(_) => Vec::new(),
    }
}

/// Normalize a MAC for matching: lowercase, strip `:` / `-` / spaces.
fn norm_mac(mac: &str) -> String {
    mac.to_ascii_lowercase()
        .replace([':', '-', ' '], "")
}

fn is_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6 && parts.iter().all(|p| p.len() == 2 && p.bytes().all(|c| c.is_ascii_hexdigit()))
}

/// Parse the wire MAC from a PVE QEMU net device string, e.g.
/// `e1000=AA:BB:CC:DD:EE:FF,bridge=vmbr0`. An explicit `mac=` option wins;
/// otherwise the first MAC-shaped value (the `model=MAC` part) is used.
fn qemu_net_mac(dev: &str) -> Option<String> {
    let mut override_mac = None;
    let mut first = None;
    for kv in dev.split(',') {
        let (k, v) = kv.split_once('=')?;
        let k = k.trim();
        let v = v.trim();
        if k == "mac" {
            override_mac = Some(v.to_string());
        } else if first.is_none() && is_mac(v) {
            first = Some(v.to_string());
        }
    }
    override_mac.or(first)
}

/// Parse the wire MAC from a PVE LXC net string, e.g.
/// `name="eth0",hwaddr=AA:BB:CC:DD:EE:FF,ip=...`.
fn lxc_net_mac(dev: &str) -> Option<String> {
    for kv in dev.split(',') {
        let (k, v) = kv.split_once('=')?;
        if k.trim() == "hwaddr" {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn guest_port(
    remote: &Remote,
    node: &str,
    vmid: u32,
    name: &str,
    macs: &[String],
    mac_map: &HashMap<String, (Option<String>, Option<i32>)>,
) -> OmadaGuestPort {
    let mut ports = Vec::new();
    for mac in macs {
        if let Some((switch, port)) = mac_map.get(&norm_mac(mac)) {
            ports.push(OmadaGuestPortMatch {
                mac: mac.clone(),
                switch: switch.clone(),
                port: *port,
            });
        }
    }
    OmadaGuestPort {
        remote: remote.id.clone(),
        node: node.to_string(),
        vmid,
        name: name.to_string(),
        macs: macs.to_vec(),
        ports,
    }
}

#[api(
    input: {
        properties: {
            site: {
                type: String,
                optional: true,
                description: "The Omada site name. Defaults to the controller's default site.",
            }
        }
    },
    access: {
        permission: &Permission::Privilege(&["system"], PRIV_SYS_AUDIT, false),
        description: "List PVE guests with the Omada switch port each is on (if any).",
    }
)]
/// List every guest on every PVE remote with the Omada switch port(s) its NIC
/// MACs are currently on, if any.
pub async fn list_guest_ports(
    site: Option<String>,
    _rpcenv: &mut dyn RpcEnvironment,
    #[state] app: PdmApplication,
) -> Result<Vec<OmadaGuestPort>, Error> {
    let site = site.unwrap_or_default();
    let client = omada_client(&app)?;
    let upstream = upstream_site(&site);

    // Omada client list -> mac -> (switch_name, port).
    let clients: Vec<OmadaClient> = super::memo_get(&format!("clients:{site}"), {
        let client = Arc::clone(&client);
        async move { client.clients(upstream).await }
    })
    .await?;
    // switch_mac -> device name, as a fallback for when a client's switchName
    // is empty or generic.
    let devices: Vec<OmadaDevice> = super::memo_get(&format!("devices:{site}"), {
        let client = Arc::clone(&client);
        async move { client.devices(upstream).await }
    })
    .await?;
    let mut switch_names: HashMap<String, String> = HashMap::new();
    for dev in &devices {
        if dev.device_type.as_deref() == Some("switch") {
            if let (Some(mac), Some(name)) = (&dev.mac, &dev.name) {
                switch_names.insert(norm_mac(mac), name.clone());
            }
        }
    }

    let mut mac_map: HashMap<String, (Option<String>, Option<i32>)> = HashMap::new();
    for c in &clients {
        if let Some(mac) = &c.mac {
            let switch = c
                .switch_name
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    c.switch_mac
                        .as_ref()
                        .and_then(|sm| switch_names.get(&norm_mac(sm)).cloned())
                });
            mac_map.insert(norm_mac(mac), (switch, c.port));
        }
    }

    // Walk PVE remotes -> nodes -> guests; read NIC MACs; join.
    let (remotes, _digest) = app.remote_config().read()?;
    let mut out: Vec<OmadaGuestPort> = Vec::new();
    for remote in remotes.values().filter(|r| r.ty == RemoteType::Pve) {
        let Ok(pve) = app.client_factory().make_pve_client(remote) else {
            continue;
        };
        let Ok(nodes) = pve.list_nodes().await else {
            continue;
        };
        for node in nodes {
            let node_name = node.node;
            for vm in pve.list_qemu(&node_name, None).await.ok().unwrap_or_default() {
                let key = format!("qemu:{}:{}:{}", remote.id, node_name, vm.vmid);
                let macs = macs_get(&key, async {
                    let cfg = pve
                        .qemu_get_config(&node_name, vm.vmid, None, None)
                        .await?;
                    Ok(cfg
                        .net
                        .values()
                        .filter_map(|d| qemu_net_mac(d))
                        .collect::<Vec<_>>())
                })
                .await;
                out.push(guest_port(
                    remote,
                    &node_name,
                    vm.vmid,
                    vm.name.as_deref().unwrap_or_default(),
                    &macs,
                    &mac_map,
                ));
            }
            for ct in pve.list_lxc(&node_name).await.ok().unwrap_or_default() {
                let key = format!("lxc:{}:{}:{}", remote.id, node_name, ct.vmid);
                let macs = macs_get(&key, async {
                    let cfg = pve
                        .lxc_get_config(&node_name, ct.vmid, None, None)
                        .await?;
                    Ok(cfg
                        .net
                        .values()
                        .filter_map(|d| lxc_net_mac(d))
                        .collect::<Vec<_>>())
                })
                .await;
                out.push(guest_port(
                    remote,
                    &node_name,
                    ct.vmid,
                    ct.name.as_deref().unwrap_or_default(),
                    &macs,
                    &mac_map,
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_mac_lowercases_and_strips() {
        assert_eq!(norm_mac("AA:BB-CC DD:EE:FF"), "aabbccddeeff");
    }

    #[test]
    fn is_mac_accepts_only_well_formed() {
        assert!(is_mac("AA:BB:CC:DD:EE:FF"));
        assert!(is_mac("aa:bb:cc:dd:ee:ff"));
        assert!(!is_mac("AA:BB:CC:DD:EE"));
        assert!(!is_mac("GG:BB:CC:DD:EE:FF"));
        assert!(!is_mac("AA:BBBB:CC:DD:EE:FF"));
    }

    #[test]
    fn qemu_net_mac_reads_model_and_override() {
        assert_eq!(
            qemu_net_mac("e1000=AA:BB:CC:DD:EE:FF,bridge=vmbr0,firewall=1"),
            Some("AA:BB:CC:DD:EE:FF".into())
        );
        // explicit mac= wins over the model address
        assert_eq!(
            qemu_net_mac("virtio=11:22:33:44:55:66,mac=77:88:99:AA:BB:CC,bridge=vmbr0"),
            Some("77:88:99:AA:BB:CC".into())
        );
        // no MAC anywhere (auto-assigned) -> none
        assert_eq!(qemu_net_mac("e1000,bridge=vmbr0"), None);
        // rate values are not MACs
        assert_eq!(
            qemu_net_mac("virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr0,rate=100"),
            Some("AA:BB:CC:DD:EE:FF".into())
        );
    }

    #[test]
    fn lxc_net_mac_reads_hwaddr() {
        assert_eq!(
            lxc_net_mac("name=\"eth0\",hwaddr=AA:BB:CC:DD:EE:FF,ip=dhcp"),
            Some("AA:BB:CC:DD:EE:FF".into())
        );
        assert_eq!(lxc_net_mac("name=\"eth0\",ip=dhcp"), None);
    }
}
