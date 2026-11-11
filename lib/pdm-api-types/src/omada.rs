//! Serde types and the client trait for the Omada (TP-Link) OpenAPI v1 surface.
//!
//! All wire types are permissive by design: `#[serde(default)]` on every field
//! and unknown fields ignored, so Omada controller schema drift does not break
//! parsing (see spec "Verified Omada OpenAPI v1 facts").

use async_trait::async_trait;

use proxmox_schema::api;

/// The four read-only list endpoints the PDM server proxies.
///
/// `site` is an optional site id override; `None` means "the client's default
/// site" (resolved and cached by the concrete client).
#[async_trait]
pub trait OmadaApiClient: Send + Sync {
    async fn vlans(&self, site: Option<&str>) -> Result<Vec<OmadaVlan>, anyhow::Error>;
    async fn devices(&self, site: Option<&str>) -> Result<Vec<OmadaDevice>, anyhow::Error>;
    async fn switch_ports(
        &self,
        site: Option<&str>,
        mac: &str,
    ) -> Result<Vec<OmadaPort>, anyhow::Error>;
    async fn clients(&self, site: Option<&str>) -> Result<Vec<OmadaClient>, anyhow::Error>;
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmadaSite {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmadaVlan {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub vlan: Option<u32>,
    #[serde(default)]
    pub gateway_subnet: Option<String>,
    #[serde(default)]
    pub dhcp: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmadaPort {
    #[serde(default)]
    pub port: Option<i32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub profile_name: Option<String>,
    #[serde(default)]
    pub poe_mode: Option<String>,
    #[serde(default)]
    pub lag_port: Option<bool>,
    #[serde(default)]
    pub status: Option<i32>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmadaDevice {
    #[serde(default)]
    pub id: Option<String>,
    /// Wire key is `type` (gateway/switch/ap).
    #[serde(default, rename = "type")]
    pub device_type: Option<String>,
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub firmware: Option<String>,
    #[serde(default)]
    pub status: Option<i32>,
    #[serde(default)]
    pub port_list: Option<Vec<OmadaPort>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmadaClient {
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub mac: Option<String>,
    /// Wire key is `hostName`.
    #[serde(default, rename = "hostName")]
    pub hostname: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub vid: Option<u32>,
    #[serde(default)]
    pub port: Option<i32>,
    #[serde(default)]
    pub switch_mac: Option<String>,
    #[serde(default)]
    pub switch_name: Option<String>,
    #[serde(default)]
    pub ap_name: Option<String>,
    #[serde(default)]
    pub connect_type: Option<i32>,
    #[serde(default)]
    pub network_name: Option<String>,
    #[serde(default)]
    pub blocked: Option<bool>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub last_seen: Option<i64>,
}

/// PDM annotation: the client matches a PVE remote node.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PveMatch {
    pub remote: String,
    pub node: Option<String>,
}

/// An Omada client plus the optional PVE-remote annotation (flat, not nested,
/// so the UI can render it in one row).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OmadaClientEntry {
    #[serde(flatten)]
    pub client: OmadaClient,
    #[serde(default, flatten, skip_serializing_if = "Option::is_none")]
    pub pve: Option<PveMatch>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OmadaDevicesView {
    pub devices: Vec<OmadaDevice>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
/// The Omada controller configuration as seen by the API (the client secret
/// is write-only; `secret_set` indicates whether one is stored).
#[api]
pub struct OmadaConfigView {
    /// Base URL of the Omada controller.
    pub url: Option<String>,
    /// The controller's omadacId, if known.
    pub omadac_id: Option<String>,
    /// The OpenAPI client ID.
    pub client_id: Option<String>,
    /// Whether a client secret is stored.
    pub secret_set: bool,
    /// Digest of the stored config, for optimistic concurrency.
    pub digest: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OmadaConfigPut {
    pub url: String,
    pub omadac_id: Option<String>,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub digest: Option<String>,
}

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlan_parses_probe_shape_and_ignores_unknowns() {
        let v: OmadaVlan = serde_json::from_str(
            r#"{"id":"17","name":"servers","vlan":12,"vlanType":0,
                "gatewaySubnet":"172.31.12.0/24","dhcpSettingsVO":{"dhcp":true}}"#,
        )
        .unwrap();
        assert_eq!(v.vlan, Some(12));
        assert_eq!(v.name.as_deref(), Some("servers"));
        assert_eq!(v.gateway_subnet.as_deref(), Some("172.31.12.0/24"));
    }

    #[test]
    fn client_parses_minimal_record() {
        let c: OmadaClient =
            serde_json::from_str(r#"{"ip":"172.31.12.97","mac":"aa:bb:cc:dd:ee:ff"}"#).unwrap();
        assert_eq!(c.ip.as_deref(), Some("172.31.12.97"));
        assert_eq!(c.switch_mac, None);
        assert_eq!(c.port, None);
    }

    #[test]
    fn client_entry_serializes_flat_with_pve() {
        let mut e: OmadaClientEntry =
            serde_json::from_str(r#"{"ip":"172.31.12.97","hostName":"pve"}"#).unwrap();
        e.pve = Some(PveMatch {
            remote: "pve-lab".into(),
            node: None,
        });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["remote"], "pve-lab"); // flat, no "pve" nesting
        assert_eq!(v["ip"], "172.31.12.97");
    }

    #[test]
    fn device_parses_type_and_port_list() {
        let d: OmadaDevice = serde_json::from_str(
            r#"{"id":"1","type":"switch","mac":"28-87-BA-61-24-DE","name":"SG3428",
                "portList":[{"port":17,"profileId":"p1","profileName":"servers","status":1}]}"#,
        )
        .unwrap();
        assert_eq!(d.device_type.as_deref(), Some("switch"));
        assert_eq!(d.port_list.as_ref().unwrap()[0].port, Some(17));
    }
}
