//! Read-only endpoints for the configured Omada controller.
//!
//! All upstream Omada data is memoized for a short TTL (the UI auto-refreshes),
//! so a 5-second UI refresh does not hammer the controller. Only the
//! *upstream* data is memoized; the PVE-remote client annotation is recomputed
//! on every call because it is cheap and the remote mapping cache changes
//! independently.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Error};
use pdm_api_types::omada::{
    OmadaApiClient, OmadaClient, OmadaClientEntry, OmadaConfigPut, OmadaConfigView, OmadaDevice,
    OmadaDevicesView, OmadaVlan,
};
use pdm_api_types::{PRIV_SYS_AUDIT, PRIV_SYS_MODIFY};
use pdm_config::omada::OmadaRemote;
use proxmox_config_digest::ConfigDigest;
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

/// Process-wide client cache: one client per configuration digest, so the
/// token/site/omdadacId caches outlive a single request and are only rebuilt
/// when the configuration actually changes.
static CLIENT_CACHE: Mutex<Option<(ConfigDigest, Arc<dyn OmadaApiClient>)>> = Mutex::new(None);

pub(crate) fn cached_omada_client(
    remote: &OmadaRemote,
    digest: &ConfigDigest,
) -> Result<Arc<dyn OmadaApiClient>, Error> {
    let make = || -> Result<Arc<dyn OmadaApiClient>, Error> {
        Ok(Arc::new(http::OmadaHttpClient::new(remote)?))
    };

    let mut guard = match CLIENT_CACHE.lock() {
        Ok(guard) => guard,
        // Poisoned lock: fall back to a fresh client.
        Err(_) => return make(),
    };

    if let Some((cached_digest, client)) = guard.as_ref() {
        if cached_digest == digest {
            return Ok(Arc::clone(client));
        }
    }

    let client = make()?;
    *guard = Some((digest.clone(), Arc::clone(&client)));
    Ok(client)
}

/// Drop all memoized upstream responses (used by tests).
pub async fn flush_memo() {
    memo().entries.lock().await.clear();
}

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

/// The upstream site override: an empty/omitted site means "the
/// controller's default site", resolved by the client.
fn upstream_site(site: &str) -> Option<&str> {
    if site.is_empty() { None } else { Some(site) }
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
    let upstream = upstream_site(&site);

    memo_get(&format!("vlans:{site}"), async move {
        client.vlans(upstream).await
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
    let upstream = upstream_site(&site);

    let mut devices: Vec<OmadaDevice> = memo_get(&format!("devices:{site}"), async move {
        upstream_client.devices(upstream).await
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

        match client.switch_ports(upstream_site(&site), &mac).await {
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
    let upstream = upstream_site(&site);

    let clients: Vec<OmadaClient> = memo_get(&format!("clients:{site}"), async move {
        client.clients(upstream).await
    })
    .await
    .with_context(|| format!("failed to list Omada clients for site '{site}'"))?;

    let mut entries: Vec<OmadaClientEntry> = clients
        .into_iter()
        .map(|client| OmadaClientEntry { client, pve: None })
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

/// Build the config response from the stored remote, if any.
///
/// The secret itself is never included; only `secret_set` tells the caller
/// whether one is stored.
fn config_view_from(remote: Option<&OmadaRemote>, digest: &ConfigDigest) -> serde_json::Value {
    serde_json::json!({
        "url": remote.map(|r| r.url.clone()),
        "omadac_id": remote.and_then(|r| r.omadac_id.clone()),
        "client_id": remote.map(|r| r.client_id.clone()),
        "secret_set": remote.and_then(|r| r.client_secret.as_ref()).is_some(),
        "digest": remote.map(|_| digest.to_hex()),
    })
}

/// Merge a PUT into the current remote: an absent or empty `client_secret`
/// keeps the stored secret, anything else replaces it.
fn merge_put(current: Option<&OmadaRemote>, put: &OmadaConfigPut) -> Result<OmadaRemote, Error> {
    let client_secret = match put.client_secret.as_deref() {
        Some(secret) if !secret.is_empty() => Some(secret.to_string()),
        _ => current.and_then(|current| current.client_secret.clone()),
    };

    Ok(OmadaRemote {
        url: put.url.clone(),
        omadac_id: put.omadac_id.clone(),
        client_id: put.client_id.clone(),
        client_secret,
    })
}

#[api(
    access: {
        permission: &Permission::Privilege(&["system"], PRIV_SYS_AUDIT, false),
        description: "Read the Omada controller configuration (secret is write-only).",
    },
    returns: { type: OmadaConfigView },
)]
/// Get the Omada controller configuration.
pub fn get_config(_rpcenv: &mut dyn RpcEnvironment) -> Result<serde_json::Value, Error> {
    let (remote, digest) = pdm_config::omada::read()?;
    Ok(config_view_from(remote.as_ref(), &digest))
}

#[api(
    input: {
        properties: {
            url: {
                type: String,
                description: "Base URL of the Omada controller (e.g. https://172.31.12.45:8043).",
            },
            omadac_id: {
                type: String,
                optional: true,
                description: "The controller's omadacId; resolved automatically when absent.",
            },
            client_id: {
                type: String,
                description: "The OpenAPI client ID.",
            },
            client_secret: {
                type: String,
                optional: true,
                description: "The OpenAPI client secret; keep absent or empty to retain the stored one.",
            },
            digest: {
                type: ConfigDigest,
                optional: true,
            },
        }
    },
    access: {
        permission: &Permission::Privilege(&["system", "omada"], PRIV_SYS_MODIFY, false),
        description: "Update the Omada controller configuration.",
    }
)]
/// Update the Omada controller configuration.
pub fn update_config(
    url: String,
    omadac_id: Option<String>,
    client_id: String,
    client_secret: Option<String>,
    digest: Option<ConfigDigest>,
) -> Result<bool, Error> {
    let (current, expected_digest) = pdm_config::omada::read()?;

    if digest.is_some() {
        expected_digest.detect_modification(digest.as_ref())?;
    }

    let merged = merge_put(
        current.as_ref(),
        &OmadaConfigPut {
            url,
            omadac_id,
            client_id,
            client_secret,
            digest: None,
        },
    )?;

    pdm_config::omada::save(&merged)?;

    Ok(true)
}

pub const ROUTER: Router = Router::new()
    .get(&API_METHOD_LIST_VLANS)
    .get(&API_METHOD_LIST_DEVICES)
    .get(&API_METHOD_LIST_CLIENTS)
    .get(&API_METHOD_GET_CONFIG)
    .put(&API_METHOD_UPDATE_CONFIG);

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

    fn remote_with_secret() -> OmadaRemote {
        OmadaRemote {
            url: "https://172.31.12.45:8043".into(),
            omadac_id: Some("OM-1234".into()),
            client_id: "app-id".into(),
            client_secret: Some("old".into()),
        }
    }

    fn secret_of(remote: &OmadaRemote) -> &str {
        remote.client_secret.as_deref().expect("has a secret")
    }

    #[test]
    fn view_never_leaks_secret() {
        let v = config_view_from(Some(&remote_with_secret()), &ConfigDigest::from_slice([]));
        assert_eq!(v["secret_set"], true);
        assert!(v.get("client_secret").is_none());
        assert_eq!(v["url"], "https://172.31.12.45:8043");
        assert!(v["digest"].is_string());
    }

    #[test]
    fn view_unset_when_no_section() {
        let v = config_view_from(None, &ConfigDigest::from_slice([]));
        assert_eq!(v["secret_set"], false);
        assert!(v["url"].is_null());
        assert!(v["digest"].is_null());
    }

    #[test]
    fn put_with_absent_secret_keeps_stored_one() {
        let cur = remote_with_secret();
        let merged = merge_put(
            Some(&cur),
            &OmadaConfigPut {
                url: cur.url.clone(),
                omadac_id: None,
                client_id: cur.client_id.clone(),
                client_secret: None,
                digest: None,
            },
        )
        .unwrap();
        assert_eq!(secret_of(&merged), "old");
    }

    #[test]
    fn put_with_empty_secret_keeps_stored_one() {
        let cur = remote_with_secret();
        let merged = merge_put(
            Some(&cur),
            &OmadaConfigPut {
                url: cur.url.clone(),
                omadac_id: None,
                client_id: cur.client_id.clone(),
                client_secret: Some(String::new()),
                digest: None,
            },
        )
        .unwrap();
        assert_eq!(secret_of(&merged), "old");
    }

    #[test]
    fn put_with_new_secret_replaces() {
        let cur = remote_with_secret();
        let merged = merge_put(
            Some(&cur),
            &OmadaConfigPut {
                url: cur.url.clone(),
                omadac_id: None,
                client_id: cur.client_id.clone(),
                client_secret: Some("new".into()),
                digest: None,
            },
        )
        .unwrap();
        assert_eq!(secret_of(&merged), "new");
    }

    #[test]
    fn put_without_stored_secret_stays_absent() {
        let merged = merge_put(
            None,
            &OmadaConfigPut {
                url: "https://omada:8043".into(),
                omadac_id: None,
                client_id: "id".into(),
                client_secret: None,
                digest: None,
            },
        )
        .unwrap();
        assert_eq!(merged.client_secret, None);
    }

    #[test]
    fn stale_digest_is_rejected() {
        let expected = ConfigDigest::from_slice(b"current");
        let stale = ConfigDigest::from_slice(b"other");
        assert!(expected.detect_modification(Some(&stale)).is_err());
    }
}
