//! Read-only endpoints for the configured Omada controller.
//!
//! All upstream Omada data is memoized for a short TTL (the UI auto-refreshes),
//! so a 5-second UI refresh does not hammer the controller. Only the
//! *upstream* data is memoized; the PVE-remote client annotation is recomputed
//! on every call because it is cheap and the remote mapping cache changes
//! independently.

use std::collections::HashMap;
use std::future::Future;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Error};
use pdm_api_types::omada::{
    OmadaApiClient, OmadaClient, OmadaClientEntry, OmadaDevice, OmadaDevicesView, OmadaVlan,
};
use pdm_api_types::PRIV_SYS_AUDIT;
use proxmox_router::{Permission, Router, RpcEnvironment};
use proxmox_schema::api;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use crate::api::omada::join::{
    annotate_client, build_dns_map, build_match_map, remote_join_entries, std_resolver,
};
use crate::context::PdmApplication;

pub mod http;
pub mod join;

/// Memoize upstream Omada responses for a short TTL.
struct Memo {
    ttl: Duration,
    entries: AsyncMutex<HashMap<String, (Instant, serde_json::Value)>>,
}

static MEMO: OnceLock<Memo> = OnceLock::new();

fn memo() -> &'static Memo {
    MEMO.get_or_init(|| Memo {
        ttl: Duration::from_secs(10),
        entries: AsyncMutex::new(HashMap::new()),
    })
}

/// Return the memoized value for `key` if still fresh, otherwise run `f`,
/// memoize its result and return it.
async fn memo_get<T: Serialize + for<'de> Deserialize<'de>>(
    key: &str,
    f: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    {
        let entries = memo().entries.lock().await;
        if let Some((at, value)) = entries.get(key) {
            if at.elapsed() < memo().ttl {
                return Ok(serde_json::from_value(value.clone())?);
            }
        }
    }

    let value = f.await?;
    let serialized = serde_json::to_value(&value)?;
    memo()
        .entries
        .lock()
        .await
        .insert(key.to_string(), (Instant::now(), serialized.clone()));
    Ok(serde_json::from_value(serialized)?)
}

/// The effective Omada client from the application, or an error telling the
/// user to configure one.
fn omada_client(app: &PdmApplication) -> Result<std::sync::Arc<dyn OmadaApiClient>, Error> {
    app.client_factory()
        .make_omada_client()?
        .context("no Omada controller configured, add one under /omada/config")
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
        description: "Read the Omada VLAN list.",
    }
)]
/// List VLANs of the configured Omada controller.
pub async fn list_vlans(
    site: Option<String>,
    _rpcenv: &mut dyn RpcEnvironment,
    #[state] app: PdmApplication,
) -> Result<Vec<OmadaVlan>, Error> {
    let site = site.unwrap_or_default();
    let client = omada_client(&app)?;
    let upstream_site = site.clone();

    memo_get(&format!("vlans:{site}"), async move {
        client.vlans(Some(&upstream_site)).await
    })
    .await
    .with_context(|| format!("failed to list Omada VLANs for site '{site}'"))
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
        description: "Read the Omada device list (with switch ports).",
    }
)]
/// List devices of the configured Omada controller, attaching switch ports
/// where available.
pub async fn list_devices(
    site: Option<String>,
    _rpcenv: &mut dyn RpcEnvironment,
    #[state] app: PdmApplication,
) -> Result<OmadaDevicesView, Error> {
    let site = site.unwrap_or_default();
    let client = omada_client(&app)?;

    // Cache the upstream data only; the per-switch port fan-out (and its
    // warnings) is recomputed on every call.
    let upstream_client = std::sync::Arc::clone(&client);
    let upstream_site = site.clone();

    let mut devices: Vec<OmadaDevice> =
        memo_get(&format!("devices:{site}"), async move {
            upstream_client.devices(Some(&upstream_site)).await
        })
        .await
        .with_context(|| format!("failed to list Omada devices for site '{site}'"))?;

    let mut warnings = Vec::new();

    for device in devices.iter_mut() {
        if device.device_type.as_deref() != Some("switch") {
            continue;
        }

        let Some(mac) = device.mac.clone() else {
            continue;
        };

        match client.switch_ports(Some(&site), &mac).await {
            Ok(ports) => device.port_list = Some(ports),
            Err(err) => warnings.push(format!("switch {mac}: {err}")),
        }
    }

    Ok(OmadaDevicesView { devices, warnings })
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
        description: "Read the Omada client list, annotated with PVE remotes where matched.",
    }
)]
/// List clients of the configured Omada controller, annotated with the PVE
/// remote/node they belong to (when one matches by IP or host name).
pub async fn list_clients(
    site: Option<String>,
    _rpcenv: &mut dyn RpcEnvironment,
    #[state] app: PdmApplication,
) -> Result<Vec<OmadaClientEntry>, Error> {
    let site = site.unwrap_or_default();
    let client = omada_client(&app)?;
    let upstream_site = site.clone();

    let clients: Vec<OmadaClient> =
        memo_get(&format!("clients:{site}"), async move {
            client.clients(Some(&upstream_site)).await
        })
        .await
        .with_context(|| format!("failed to list Omada clients for site '{site}'"))?;

    let mut entries: Vec<OmadaClientEntry> = clients
        .into_iter()
        .map(|client| OmadaClientEntry {
            client,
            pve: None,
        })
        .collect();

    annotate_entries(&mut entries, &app).await?;

    Ok(entries)
}

/// Build the match map from the PVE remotes + remote mapping cache + DNS and
/// annotate each client entry in place.
async fn annotate_entries(
    entries: &mut [OmadaClientEntry],
    app: &PdmApplication,
) -> Result<(), Error> {
    let (remotes, _digest) = app.remote_config().read()?;
    let remotes = remote_join_entries(&remotes);

    // Resolve DNS for every non-IP-literal host name, swallowing per-host
    // errors (a dead DNS must not break the endpoint).
    let hosts: Vec<String> = remotes
        .iter()
        .flat_map(|remote| remote.nodes.iter().map(|node| node.hostname.clone()))
        .filter(|host| host.parse::<std::net::IpAddr>().is_err())
        .collect();

    let dns = tokio::task::spawn_blocking(move || build_dns_map(&hosts, std_resolver))
        .await
        .unwrap_or_else(|_| HashMap::new());

    let map = build_match_map(&remotes, &dns);

    for entry in entries.iter_mut() {
        annotate_client(entry, &map);
    }

    Ok(())
}

pub const ROUTER: Router = Router::new()
    .get(&API_METHOD_LIST_VLANS)
    .get(&API_METHOD_LIST_DEVICES)
    .get(&API_METHOD_LIST_CLIENTS);

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[tokio::test]
    async fn memo_returns_cached_value_within_ttl() {
        let calls = AtomicUsize::new(0);
        let key = format!("test:{}", std::process::id());

        let value: i32 = memo_get(&key, async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(42)
        })
        .await
        .unwrap();
        assert_eq!(value, 42);

        let value: i32 = memo_get(&key, async {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(43)
        })
        .await
        .unwrap();
        assert_eq!(value, 42, "second call must be served from the memo");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Drop the entry so parallel tests do not see it.
        memo().entries.lock().await.remove(&key);
    }
}
