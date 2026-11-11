//! Read/write the single Omada controller config (`omada.cfg` + `omada.shadow`).
//!
//! Modeled on [`crate::remotes`]: a section-style config file plus a shadow file holding the
//! client secret (so the secret never ends up in the world-readable cfg file). There is exactly
//! one fixed section (`omada`), so the API is a single optional entry rather than a map.
//!
//! File format (default PVE section syntax):
//!
//! ```text
//! omada: omada
//! \turl https://172.31.12.45:8043
//! \tomadac-id OM-1
//! \tclient-id pdm
//! ```
//!
//! Make sure to call [`init`] once when starting up the API server.

use std::sync::{Mutex, OnceLock};

use anyhow::Error;
use proxmox_config_digest::ConfigDigest;
use proxmox_schema::{ApiStringFormat, EnumEntry, ObjectSchema, Schema, StringSchema};
use proxmox_section_config::typed::{ApiSectionDataEntry, SectionConfigData};
use proxmox_section_config::{SectionConfig, SectionConfigPlugin};
use serde::{Deserialize, Serialize};

use pdm_buildcfg::configdir;

pub const OMADA_CFG_FILENAME: &str = configdir!("/omada.cfg");
pub const OMADA_SHADOW_FILENAME: &str = configdir!("/omada.shadow");
pub const OMADA_CFG_LOCKFILE: &str = configdir!("/.omada.lock");
pub const OMADA_SECTION_ID: &str = "omada";

/// The section id is fixed to `omada`.
pub const OMADA_SECTION_ID_FORMAT: ApiStringFormat = ApiStringFormat::Enum(&[EnumEntry::new(
    OMADA_SECTION_ID,
    "The single fixed Omada section.",
)]);
pub const OMADA_SECTION_ID_SCHEMA: Schema =
    StringSchema::new("Omada section id (fixed to \"omada\").")
        .format(&OMADA_SECTION_ID_FORMAT)
        .schema();

const OMADA_PROPERTIES: ObjectSchema = ObjectSchema::new(
    "Omada controller properties",
    &[
        (
            "client-id",
            false,
            &StringSchema::new("The OAuth client ID.").schema(),
        ),
        (
            "client-secret",
            true,
            &StringSchema::new("The OAuth client secret.").schema(),
        ),
        (
            "omadac-id",
            true,
            &StringSchema::new("The Omada controller ID (auto-resolved when absent).").schema(),
        ),
        (
            "url",
            false,
            &StringSchema::new("The controller URL, e.g. https://172.31.12.45:8043.").schema(),
        ),
    ],
);

const OMADA_SHADOW_PROPERTIES: ObjectSchema = ObjectSchema::new(
    "Omada shadow properties",
    &[(
        "client-secret",
        false,
        &StringSchema::new("The OAuth client secret.").schema(),
    )],
);

/// The Omada controller connection details.
///
/// `client_secret` is merged in from `omada.shadow` by [`read`] and stripped again by
/// [`save`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OmadaRemote {
    /// Controller URL, e.g. `https://172.31.12.45:8043` (scheme required).
    pub url: String,
    /// Optional; auto-resolved via `GET /api/info` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omadac_id: Option<String>,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

impl ApiSectionDataEntry for OmadaRemote {
    fn section_config() -> &'static SectionConfig {
        static CONFIG: OnceLock<SectionConfig> = OnceLock::new();
        CONFIG.get_or_init(|| {
            let mut this = SectionConfig::new(&OMADA_SECTION_ID_SCHEMA);
            this.register_plugin(SectionConfigPlugin::new(
                OMADA_SECTION_ID.to_string(),
                None,
                &OMADA_PROPERTIES,
            ));
            this
        })
    }

    fn section_type(&self) -> &'static str {
        OMADA_SECTION_ID
    }

    // Not internally tagged: the section value *is* the entry.
    fn from_value(_ty: String, value: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    fn to_pair(&self) -> Result<(String, serde_json::Value), serde_json::Error> {
        Ok((OMADA_SECTION_ID.to_string(), serde_json::to_value(self)?))
    }
}

/// The `omada.shadow` entry: just the client secret.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OmadaShadow {
    pub client_secret: String,
}

impl ApiSectionDataEntry for OmadaShadow {
    fn section_config() -> &'static SectionConfig {
        static CONFIG: OnceLock<SectionConfig> = OnceLock::new();
        CONFIG.get_or_init(|| {
            let mut this = SectionConfig::new(&OMADA_SECTION_ID_SCHEMA);
            this.register_plugin(SectionConfigPlugin::new(
                OMADA_SECTION_ID.to_string(),
                None,
                &OMADA_SHADOW_PROPERTIES,
            ));
            this
        })
    }

    fn section_type(&self) -> &'static str {
        OMADA_SECTION_ID
    }

    fn from_value(_ty: String, value: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }

    fn to_pair(&self) -> Result<(String, serde_json::Value), serde_json::Error> {
        Ok((OMADA_SECTION_ID.to_string(), serde_json::to_value(self)?))
    }
}

static INSTANCE: OnceLock<Mutex<()>> = OnceLock::new();

/// Initialize the module state. Panics if called twice, like `remotes::init`.
pub fn init() {
    if INSTANCE.set(Mutex::new(())).is_err() {
        panic!("omada config instance already set");
    }
}

/// Parse the cfg (and optional shadow) content; the digest is over the **cfg** content.
pub fn parse_cfg(
    cfg: &str,
    shadow: Option<&str>,
) -> Result<(Option<OmadaRemote>, ConfigDigest), Error> {
    let digest: ConfigDigest = openssl::sha::sha256(cfg.as_bytes()).into();

    let data = OmadaRemote::parse_section_config(OMADA_CFG_FILENAME, cfg)?;
    let mut remote = data.get(OMADA_SECTION_ID).cloned();

    if let Some(shadow) = shadow {
        let shadow_data = OmadaShadow::parse_section_config(OMADA_SHADOW_FILENAME, shadow)?;
        if let (Some(remote), Some(shadow_entry)) =
            (remote.as_mut(), shadow_data.get(OMADA_SECTION_ID))
        {
            if remote.client_secret.is_none() {
                remote.client_secret = Some(shadow_entry.client_secret.clone());
            }
        }
    }

    Ok((remote, digest))
}

/// Render the remote as (cfg content, shadow content). The shadow is `None` when there is no
/// secret; the rendered cfg never contains the secret.
pub fn render_cfg(remote: &OmadaRemote) -> Result<(String, Option<String>), Error> {
    let secret = remote.client_secret.clone();

    let mut cfg_entry = remote.clone();
    cfg_entry.client_secret = None;
    let mut cfg_data = SectionConfigData::default();
    cfg_data.insert(OMADA_SECTION_ID.to_string(), cfg_entry);
    let cfg = OmadaRemote::write_section_config(OMADA_CFG_FILENAME, &cfg_data)?;

    let shadow = match secret {
        Some(client_secret) => {
            let mut shadow_data = SectionConfigData::default();
            shadow_data.insert(OMADA_SECTION_ID.to_string(), OmadaShadow { client_secret });
            Some(OmadaShadow::write_section_config(
                OMADA_SHADOW_FILENAME,
                &shadow_data,
            )?)
        }
        None => None,
    };

    Ok((cfg, shadow))
}

/// Return the configured remote (secret merged from the shadow file) and the cfg digest.
pub fn read() -> Result<(Option<OmadaRemote>, ConfigDigest), Error> {
    let cfg = proxmox_sys::fs::file_read_optional_string(OMADA_CFG_FILENAME)?.unwrap_or_default();
    let shadow = proxmox_sys::fs::file_read_optional_string(OMADA_SHADOW_FILENAME)?;
    parse_cfg(&cfg, shadow.as_deref())
}

/// Persist the remote; the secret is moved to the shadow file (the shadow is removed when there
/// is no secret anymore).
pub fn save(remote: &OmadaRemote) -> Result<(), Error> {
    let _lock = INSTANCE
        .get()
        .expect("omada config not initialized")
        .lock()
        .map_err(|e| anyhow::format_err!("omada config lock poisoned: {e}"))?;

    let (cfg, shadow) = render_cfg(remote)?;

    // write out shadow config *only before* the cfg, same ordering as remotes
    match shadow {
        Some(shadow) => {
            proxmox_product_config::replace_secret_config(OMADA_SHADOW_FILENAME, shadow.as_bytes())?
        }
        None => {
            let _ = std::fs::remove_file(OMADA_SHADOW_FILENAME); // no secret -> no shadow
        }
    }
    proxmox_product_config::replace_config(OMADA_CFG_FILENAME, cfg.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_with_secret() -> OmadaRemote {
        OmadaRemote {
            url: "https://172.31.12.45:8043".into(),
            omadac_id: Some("OM-1".into()),
            client_id: "pdm".into(),
            client_secret: Some("s3cret".into()),
        }
    }

    #[test]
    fn roundtrip_cfg_and_shadow() {
        let r = remote_with_secret();
        let (cfg, shadow) = render_cfg(&r).unwrap();
        let shadow_cfg = shadow.unwrap();
        assert!(!cfg.contains("s3cret")); // secret never in omada.cfg
        assert!(shadow_cfg.contains("s3cret"));
        let (parsed_remote, _digest) = parse_cfg(&cfg, Some(&shadow_cfg)).unwrap();
        let parsed = parsed_remote.unwrap();
        assert_eq!(parsed.client_secret.as_deref(), Some("s3cret"));
        // re-render is stable
        let (cfg2, shadow2) = render_cfg(&parsed).unwrap();
        assert_eq!(cfg2, cfg);
        assert_eq!(shadow2.as_deref(), Some(shadow_cfg.as_str()));
    }

    #[test]
    fn cfg_without_shadow_reads_secret_absent() {
        let (remote, _d) = parse_cfg(
            "omada: omada\n\turl https://x:8043\n\tclient-id pdm\n",
            None,
        )
        .unwrap();
        let remote = remote.unwrap();
        assert!(remote.client_secret.is_none());
        assert_eq!(remote.omadac_id, None);
        assert_eq!(remote.url, "https://x:8043");
    }

    #[test]
    fn missing_section_is_none() {
        let (remote, _d) = parse_cfg("", None).unwrap();
        assert_eq!(remote, None);
    }
}
