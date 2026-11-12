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

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmadaSite {
    /// Wire key is `siteId` (list items); accept `id` too for drift.
    #[serde(default, rename = "siteId", alias = "id")]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
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
    /// Nested wire shape: `dhcpSettingsVO: {enable: bool, ...}`.
    #[serde(default, rename = "dhcpSettingsVO")]
    pub dhcp_settings: Option<OmadaDhcpSettings>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmadaDhcpSettings {
    /// Wire key is `enable` (Omada 6.x); accept `dhcp` for drift.
    #[serde(default, rename = "enable", alias = "dhcp")]
    pub dhcp: Option<bool>,
}

impl OmadaVlan {
    /// Whether DHCP is enabled on the VLAN.
    pub fn dhcp(&self) -> Option<bool> {
        self.dhcp_settings.and_then(|d| d.dhcp)
    }
}

/// `poeMode` is a string on some firmware revisions ("auto"/"off"/"on") and an
/// integer on others (0/1/2). Accept both and store a display string so a
/// single field never fails the whole port list parse.
fn de_poe_mode<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s),
        other => Some(other.to_string()),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
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
    #[serde(default, deserialize_with = "de_poe_mode")]
    pub poe_mode: Option<String>,
    #[serde(default)]
    pub lag_port: Option<bool>,
    #[serde(default)]
    pub status: Option<i32>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
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
    /// Wire key is `firmwareVersion` (Omada 6.x); accept `firmware` for drift.
    #[serde(default, rename = "firmwareVersion", alias = "firmware")]
    pub firmware: Option<String>,
    #[serde(default)]
    pub status: Option<i32>,
    #[serde(default)]
    pub port_list: Option<Vec<OmadaPort>>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct PveMatch {
    pub remote: String,
    pub node: Option<String>,
}

/// An Omada client plus the optional PVE-remote annotation (flat, not nested,
/// so the UI can render it in one row).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct OmadaClientEntry {
    #[serde(flatten)]
    pub client: OmadaClient,
    #[serde(default, flatten, skip_serializing_if = "Option::is_none")]
    pub pve: Option<PveMatch>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct OmadaDevicesView {
    pub devices: Vec<OmadaDevice>,
    pub warnings: Vec<String>,
}

/// One NIC of a guest matched to an Omada switch port.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmadaGuestPortMatch {
    /// The guest NIC MAC that matched an Omada client.
    pub mac: String,
    /// Name of the Omada switch, if known.
    pub switch: Option<String>,
    /// Port number on the switch.
    pub port: Option<i32>,
}

/// One guest (VM or container) and the Omada switch port(s) its NIC MACs are
/// currently on, if any. Computed server-side by joining PVE guest NIC MACs
/// (from per-guest config) with the Omada client list.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmadaGuestPort {
    /// PDM remote id.
    pub remote: String,
    /// PVE node name.
    pub node: String,
    /// Guest VMID (VM) or CTID (container).
    pub vmid: u32,
    /// Guest name.
    pub name: String,
    /// The guest's NIC MACs (best effort, from PVE config).
    #[serde(default)]
    pub macs: Vec<String>,
    /// Omada switch ports matched by those MACs (empty = none on a managed
    /// switch).
    #[serde(default)]
    pub ports: Vec<OmadaGuestPortMatch>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
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
        // Real Omada 6.x shape: `dhcpSettingsVO.enable`.
        let v: OmadaVlan = serde_json::from_str(
            r#"{"id":"17","name":"servers","vlan":12,"vlanType":0,
                "gatewaySubnet":"172.31.12.0/24",
                "dhcpSettingsVO":{"enable":true,"ipaddrStart":"172.31.12.100"}}"#,
        )
        .unwrap();
        assert_eq!(v.vlan, Some(12));
        assert_eq!(v.name.as_deref(), Some("servers"));
        assert_eq!(v.gateway_subnet.as_deref(), Some("172.31.12.0/24"));
        assert_eq!(v.dhcp(), Some(true));

        // Older / synthetic shape: `dhcpSettingsVO.dhcp` (alias).
        let off: OmadaVlan =
            serde_json::from_str(r#"{"vlan":1066,"name":"iot","dhcpSettingsVO":{"dhcp":false}}"#)
                .unwrap();
        assert_eq!(off.dhcp(), Some(false));

        let missing: OmadaVlan = serde_json::from_str(r#"{"vlan":5}"#).unwrap();
        assert_eq!(missing.dhcp(), None);
    }

    #[test]
    fn site_and_device_parse_real_keys() {
        // Sites list items carry `siteId`, not `id`.
        let s: OmadaSite =
            serde_json::from_str(r#"{"siteId":"abc123","name":"home"}"#).unwrap();
        assert_eq!(s.id.as_deref(), Some("abc123"));

        // Devices carry `firmwareVersion`, not `firmware`.
        let d: OmadaDevice = serde_json::from_str(
            r#"{"mac":"AA-BB","name":"sw","type":"switch","ip":"1.2.3.4",
                "firmwareVersion":"1.0.2","status":1}"#,
        )
        .unwrap();
        assert_eq!(d.firmware.as_deref(), Some("1.0.2"));
        assert_eq!(d.device_type.as_deref(), Some("switch"));
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

    #[test]
    fn port_parses_integer_and_string_poe_mode() {
        // Real Omada 6.x: poeMode is an integer.
        let int: OmadaPort = serde_json::from_str(
            r#"{"port":1,"name":"Port1","profileId":"p1","profileName":"All",
                "poeMode":1,"lagPort":false,"status":1}"#,
        )
        .unwrap();
        assert_eq!(int.poe_mode.as_deref(), Some("1"));

        // Older / string firmware revision.
        let str: OmadaPort =
            serde_json::from_str(r#"{"port":17,"poeMode":"auto"}"#).unwrap();
        assert_eq!(str.poe_mode.as_deref(), Some("auto"));

        let absent: OmadaPort = serde_json::from_str(r#"{"port":2}"#).unwrap();
        assert_eq!(absent.poe_mode, None);
    }
}
