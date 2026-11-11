use std::sync::Arc;

use pdm_api_types::omada::{
    OmadaApiClient, OmadaClient, OmadaDevice, OmadaPort, OmadaVlan,
};
use server::context::ContextFactory;

pub mod common;

common::test_pve_client!(OmadaPveClient {});

/// Omada client backed by the captured responses in
/// `tests/api_responses/omada/`. `switch_ports` fails for the dead switch
/// (`DE-AD-DE-AD-DE-AD`) to exercise the warning path.
struct OmadaTestClient;

#[async_trait::async_trait]
impl OmadaApiClient for OmadaTestClient {
    async fn vlans(
        &self,
        _site: Option<&str>,
    ) -> Result<Vec<OmadaVlan>, anyhow::Error> {
        Ok(common::read_captured_response("tests/api_responses/omada/vlans.json").await?)
    }

    async fn devices(
        &self,
        _site: Option<&str>,
    ) -> Result<Vec<OmadaDevice>, anyhow::Error> {
        Ok(common::read_captured_response("tests/api_responses/omada/devices.json").await?)
    }

    async fn switch_ports(
        &self,
        _site: Option<&str>,
        mac: &str,
    ) -> Result<Vec<OmadaPort>, anyhow::Error> {
        if mac == "DE-AD-DE-AD-DE-AD" {
            return Err(anyhow::anyhow!("switch is unreachable"));
        }

        Ok(
            common::read_captured_response(&format!(
                "tests/api_responses/omada/switch_{mac}.json"
            ))
            .await?,
        )
    }

    async fn clients(
        &self,
        _site: Option<&str>,
    ) -> Result<Vec<OmadaClient>, anyhow::Error> {
        Ok(common::read_captured_response("tests/api_responses/omada/clients.json").await?)
    }
}

#[tokio::test]
async fn test_clients_annotate_pve_nodes() {
    common::test_setup();

    let app = test_app().make_pdm_application().unwrap();

    let result = server::api::omada::list_clients(None, &mut common::rpcenv(), app)
        .await
        .unwrap();

    assert_eq!(result.len(), 4);

    let pve1 = result
        .iter()
        .find(|c| c.client.ip.as_deref() == Some("172.31.12.97"))
        .unwrap();
    assert_eq!(pve1.pve.as_ref().unwrap().remote, "pve-lab");

    let pve2 = result
        .iter()
        .find(|c| c.client.ip.as_deref() == Some("172.31.12.98"))
        .unwrap();
    assert!(pve2.pve.is_none(), "no node is registered for this IP");

    let wifi = result.iter().find(|c| c.client.ap_name.is_some()).unwrap();
    assert!(wifi.pve.is_none());
}

#[tokio::test]
async fn test_devices_attaches_port_list_and_collects_warnings() {
    common::test_setup();

    let app = test_app().make_pdm_application().unwrap();

    let view = server::api::omada::list_devices(None, &mut common::rpcenv(), app)
        .await
        .unwrap();

    assert_eq!(view.devices.len(), 3);
    assert_eq!(view.warnings.len(), 1);
    assert!(view.warnings[0].contains("DE-AD-DE-AD-DE-AD"));

    let healthy = view
        .devices
        .iter()
        .find(|d| d.mac.as_deref() == Some("28-87-BA-61-24-DE"))
        .unwrap();
    assert_eq!(healthy.port_list.as_ref().unwrap().len(), 3);

    let dead = view
        .devices
        .iter()
        .find(|d| d.mac.as_deref() == Some("DE-AD-DE-AD-DE-AD"))
        .unwrap();
    assert!(dead.port_list.is_none());

    let gateway = view
        .devices
        .iter()
        .find(|d| d.device_type.as_deref() == Some("gateway"))
        .unwrap();
    assert!(gateway.port_list.is_none(), "non-switches get no port fan-out");
}

#[tokio::test]
async fn test_vlans_passthrough() {
    common::test_setup();

    let app = test_app().make_pdm_application().unwrap();

    let vlans = server::api::omada::list_vlans(None, &mut common::rpcenv(), app)
        .await
        .unwrap();

    assert_eq!(vlans.len(), 2);
    let servers = vlans.iter().find(|v| v.vlan == Some(12)).unwrap();
    assert_eq!(servers.name.as_deref(), Some("servers"));
}

#[tokio::test]
async fn test_clients_requires_configured_controller() {
    common::test_setup();

    let test_app = common::TestApplication::new();
    let app = test_app.make_pdm_application().unwrap();

    let err = server::api::omada::list_clients(None, &mut common::rpcenv(), app)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("no Omada controller configured"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn test_config_endpoints() {
    common::test_setup();

    // The endpoints read/write the global config location; create the
    // directory and clear leftovers from a previous run first.
    setup_config_dir();
    clean_config_files();

    // Unconfigured: all fields null, no secret.
    let view: serde_json::Value =
        server::api::omada::get_config(&mut common::rpcenv()).unwrap();
    assert!(view["url"].is_null());
    assert_eq!(view["secret_set"], false);
    assert!(view["digest"].is_null());
    assert!(view.get("client_secret").is_none());

    // Configure with a secret.
    let stored = server::api::omada::update_config(
        "https://172.31.12.45:8043".into(),
        Some("OM-1234".into()),
        "app-id".into(),
        Some("s3cret".into()),
        None,
    );
    assert_eq!(stored.unwrap(), true);

    let view: serde_json::Value =
        server::api::omada::get_config(&mut common::rpcenv()).unwrap();
    assert_eq!(view["url"], "https://172.31.12.45:8043");
    assert_eq!(view["omadac_id"], "OM-1234");
    assert_eq!(view["client_id"], "app-id");
    assert_eq!(view["secret_set"], true);
    assert!(view.get("client_secret").is_none());
    assert!(view["digest"].is_string());

    let (stored_remote, digest) = pdm_config::omada::read().unwrap();
    let stored_remote = stored_remote.unwrap();
    assert_eq!(stored_remote.client_secret.as_deref(), Some("s3cret"));

    // Update with an empty secret: the stored one is kept.
    let stored = server::api::omada::update_config(
        "https://172.31.12.45:8043".into(),
        None,
        "app-id".into(),
        Some(String::new()),
        Some(digest),
    );
    assert_eq!(stored.unwrap(), true);
    let (stored_remote, _digest) = pdm_config::omada::read().unwrap();
    assert_eq!(
        stored_remote.unwrap().client_secret.as_deref(),
        Some("s3cret"),
        "empty secret in the PUT must keep the stored one"
    );

    // A stale digest is rejected: capture the digest, change the config,
    // then PUT again with the now-stale digest.
    let (current_remote, current_digest) = pdm_config::omada::read().unwrap();
    server::api::omada::update_config(
        "https://172.31.12.45:8043".into(),
        Some("OM-9999".into()),
        current_remote.unwrap().client_id.clone(),
        None,
        Some(current_digest.clone()),
    )
    .unwrap();
    let stale = server::api::omada::update_config(
        "https://172.31.12.45:8043".into(),
        None,
        "app-id".into(),
        None,
        Some(current_digest),
    );
    assert!(stale.is_err(), "stale digest must be rejected");

    clean_config_files();
}

fn clean_config_files() {
    let _ = std::fs::remove_file(pdm_config::omada::OMADA_CFG_FILENAME);
    let _ = std::fs::remove_file(pdm_config::omada::OMADA_SHADOW_FILENAME);
}

fn setup_config_dir() {
    // The config location comes from the build config; in tests the directory
    // may not exist yet (packaging creates it in production). It must be
    // writable by the test user.
    let parent = std::path::Path::new(pdm_config::omada::OMADA_CFG_FILENAME)
        .parent()
        .expect("config file has a parent directory");
    std::fs::create_dir_all(parent).unwrap_or_else(|err| {
        panic!(
            "could not create config dir {}: {err} (in a test environment it must exist and be writable)"
            ,
            parent.display()
        )
    });
}

/// The shared test application wiring: one PVE remote with a single node
/// (172.31.12.97) and the fixture-backed Omada client.
fn test_app() -> common::TestApplication {
    common::TestApplication::new()
        .with_pve_remote_hosted(
            "pve-lab",
            vec!["172.31.12.97".into()],
            Default::default(),
            OmadaPveClient,
        )
        .with_omada_client(|| Arc::new(OmadaTestClient))
}
