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
