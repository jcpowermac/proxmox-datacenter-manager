//! Read-only Omada controller overview.
//!
//! Four stacked panels — VLANs, switch ports, clients, devices — fed by the
//! `/omada/{vlans,devices,clients}` endpoints, plus the controller
//! configuration dialog. The data auto-refreshes every 60 seconds; the server
//! memoizes the upstream responses for 10 seconds, so the extra traffic is
//! negligible. Clients matched to a PDM PVE remote get a badge in the
//! clients and switch-port tables.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use anyhow::Error;
use futures::try_join;
use gloo_timers::callback::Interval;
use pwt::css::FlexFit;
use pwt::prelude::*;
use pwt::state::Store;
use pwt::widget::data_table::{DataTable, DataTableColumn, DataTableHeader};
use pwt::widget::form::{Field, FormContext, InputType};
use pwt::widget::{
    Badge, Button, Column, Container, InputPanel, Panel, Progress, Toolbar, Tooltip, Trigger,
    error_message,
};
use yew::virtual_dom::{Key, VComp, VNode};

use pdm_api_types::omada::{
    OmadaClient, OmadaClientEntry, OmadaConfigView, OmadaDevice, OmadaDevicesView, OmadaVlan,
    PveMatch,
};
use proxmox_client::ApiResponseData;
use proxmox_yew_comp::{
    EditWindow, LoadableComponent, LoadableComponentContext, LoadableComponentMaster,
    LoadableComponentScopeExt, LoadableComponentState, http_get, http_get_full, http_put,
};

/// A switch port row: one per port of one switch, with the client (if any)
/// that is currently on it.
#[derive(Clone, PartialEq)]
struct PortRow {
    switch_name: String,
    switch_mac: String,
    port_index: i32,
    port_name: String,
    profile_name: String,
    poe_mode: String,
    client: Option<OmadaClientEntry>,
}

type OmadaData = (Vec<OmadaVlan>, OmadaDevicesView, Vec<OmadaClientEntry>);

enum Msg {
    Loaded(Option<OmadaConfigView>, Option<Result<OmadaData, String>>),
    SetClientFilter(String),
    Reload,
}

#[derive(PartialEq)]
pub enum ViewState {
    Config,
}

#[derive(PartialEq, Properties, Clone)]
pub struct OmadaView;

impl OmadaView {
    pub fn new() -> Self {
        yew::props!(Self {})
    }
}

impl Default for OmadaView {
    fn default() -> Self {
        Self::new()
    }
}

impl From<OmadaView> for VNode {
    fn from(val: OmadaView) -> Self {
        let comp = VComp::new::<LoadableComponentMaster<OmadaViewComp>>(Rc::new(val), None);
        VNode::from(comp)
    }
}

struct OmadaViewComp {
    state: LoadableComponentState<ViewState>,
    config: Option<OmadaConfigView>,
    data: Option<Result<OmadaData, String>>,
    loading: bool,
    client_filter: String,
    vlans: Store<OmadaVlan>,
    ports: Store<PortRow>,
    clients: Store<OmadaClientEntry>,
    devices: Store<OmadaDevice>,
    vlan_columns: Rc<Vec<DataTableHeader<OmadaVlan>>>,
    port_columns: Rc<Vec<DataTableHeader<PortRow>>>,
    client_columns: Rc<Vec<DataTableHeader<OmadaClientEntry>>>,
    device_columns: Rc<Vec<DataTableHeader<OmadaDevice>>>,
    _async_pool: pwt::AsyncPool,
    interval: Option<Interval>,
}

pwt::impl_deref_mut_property!(OmadaViewComp, state, LoadableComponentState<ViewState>);

impl OmadaViewComp {
    fn apply_data(&mut self, data: &OmadaData) {
        let (vlans, devices_view, clients) = data;

        self.vlans.set_data(vlans.clone());
        self.devices.set_data(devices_view.devices.clone());
        self.clients
            .set_data(filtered_clients(clients, &self.client_filter));

        // One row per port per switch, joined with the client on that port.
        let mut rows: Vec<PortRow> = Vec::new();
        for device in &devices_view.devices {
            let Some(ports) = &device.port_list else {
                continue;
            };
            let mac = device.mac.clone().unwrap_or_default();
            let name = device.name.clone().unwrap_or_default();
            for port in ports {
                let client = clients
                    .iter()
                    .find(|client| {
                        client.client.switch_mac.as_deref() == Some(&mac)
                            && client.client.port == port.port
                    })
                    .cloned();
                rows.push(PortRow {
                    switch_name: name.clone(),
                    switch_mac: mac.clone(),
                    port_index: port.port.unwrap_or(-1),
                    port_name: port.name.clone().unwrap_or_default(),
                    profile_name: port.profile_name.clone().unwrap_or_default(),
                    poe_mode: port.poe_mode.clone().unwrap_or_default(),
                    client,
                });
            }
        }
        self.ports.set_data(rows);

        // The vlan/client columns capture lookup maps; rebuild them per load.
        let client_count: HashMap<u32, usize> = vlans
            .iter()
            .filter_map(|vlan| vlan.vlan)
            .map(|vlan_id| {
                (
                    vlan_id,
                    clients
                        .iter()
                        .filter(|client| client.client.vid == Some(vlan_id))
                        .count(),
                )
            })
            .collect();
        let vlan_names: HashMap<u32, String> = vlans
            .iter()
            .filter_map(|vlan| Some((vlan.vlan?, vlan.name.clone()?)))
            .collect();

        self.vlan_columns = vlan_columns(Rc::new(client_count));
        self.client_columns = client_columns(Rc::new(vlan_names));
    }

    fn reload(&mut self, ctx: &LoadableComponentContext<Self>) {
        self.loading = true;
        let link = ctx.link().clone();
        let inner = link.clone();
        link.spawn(async move {
            let msg = match load_omada_data().await {
                Ok((config, data)) => Msg::Loaded(Some(config), data),
                Err(err) => Msg::Loaded(None, Some(Err(err.to_string()))),
            };
            inner.send_message(msg);
        });
    }
}

impl LoadableComponent for OmadaViewComp {
    type Properties = OmadaView;
    type Message = Msg;
    type ViewState = ViewState;

    fn create(ctx: &LoadableComponentContext<Self>) -> Self {
        let mut this = Self {
            state: LoadableComponentState::new(),
            config: None,
            data: None,
            loading: true,
            client_filter: String::new(),
            vlans: Store::with_extract_key(|vlan: &OmadaVlan| {
                Key::from(format!(
                    "{}:{}",
                    vlan.vlan.unwrap_or(0),
                    vlan.id.clone().unwrap_or_default()
                ))
            }),
            ports: Store::with_extract_key(|row: &PortRow| {
                Key::from(format!("{}:{}", row.switch_mac, row.port_index))
            }),
            // The controller has no stable client id; mac (unique per
            // hardware) is the best key, falling back to ip/hostname/name.
            clients: Store::with_extract_key(|entry: &OmadaClientEntry| {
                Key::from(
                    entry
                        .client
                        .mac
                        .clone()
                        .or_else(|| entry.client.ip.clone())
                        .or_else(|| entry.client.hostname.clone())
                        .or_else(|| entry.client.name.clone())
                        .unwrap_or_default(),
                )
            }),
            devices: Store::with_extract_key(|device: &OmadaDevice| {
                Key::from(
                    device
                        .mac
                        .clone()
                        .unwrap_or_else(|| device.id.clone().unwrap_or_default()),
                )
            }),
            vlan_columns: vlan_columns(Rc::new(HashMap::new())),
            port_columns: port_columns(),
            client_columns: client_columns(Rc::new(HashMap::new())),
            device_columns: device_columns(),
            _async_pool: pwt::AsyncPool::new(),
            interval: None,
        };

        this.interval = Some(Interval::new(60_000, {
            let link = ctx.link().clone();
            move || link.send_message(Msg::Reload)
        }));

        this.reload(ctx);
        this
    }

    fn update(&mut self, ctx: &LoadableComponentContext<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::Reload => {
                self.reload(ctx);
                true
            }
            Msg::SetClientFilter(filter) => {
                self.client_filter = filter;
                if let Some(Ok((_, _, clients))) = self.data.as_ref() {
                    self.clients
                        .set_data(filtered_clients(clients, &self.client_filter));
                }
                true
            }
            Msg::Loaded(config, data) => {
                if let Some(config) = config {
                    self.config = Some(config);
                }
                self.data = data;
                self.loading = false;
                if let Some(Ok(data)) = self.data.clone() {
                    self.apply_data(&data);
                }
                true
            }
        }
    }

    fn load(
        &self,
        _ctx: &LoadableComponentContext<Self>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>>>> {
        // The data flows in via `reload`/`Msg::Loaded`; resolve immediately so
        // the LoadableComponent harness does not hold the mask up.
        Box::pin(async { Ok(()) })
    }

    fn toolbar(&self, ctx: &LoadableComponentContext<Self>) -> Option<Html> {
        let link = ctx.link();
        Some(
            Toolbar::new()
                .border_bottom(true)
                .with_child(
                    Container::new()
                        .class("pwt-font-title-medium")
                        .with_child(tr!("Omada")),
                )
                .with_flex_spacer()
                .with_child(
                    Tooltip::new(
                        Button::new(tr!("Configuration"))
                            .icon_class("fa fa-cog")
                            .on_activate(link.change_view_callback(|_| Some(ViewState::Config))),
                    )
                    .tip(tr!("Configure the Omada controller connection")),
                )
                .with_child(
                    Button::refresh(self.loading).on_activate(link.callback(|_| Msg::Reload)),
                )
                .into(),
        )
    }

    fn main_view(&self, ctx: &LoadableComponentContext<Self>) -> Html {
        let link = ctx.link();

        let error = self
            .data
            .as_ref()
            .and_then(|data| data.as_ref().err())
            .map(|err| error_message(err));

        let body: Html = match (self.config.as_ref(), self.data.as_ref()) {
            (Some(config), Some(Ok((_, devices_view, clients)))) => {
                let filter = Field::new()
                    .placeholder(tr!("Filter by IP, MAC or host name"))
                    .value(self.client_filter.clone())
                    .width("300px")
                    .with_trigger(
                        Trigger::new(if self.client_filter.is_empty() {
                            ""
                        } else {
                            "fa fa-times"
                        })
                        .tip(tr!("Clear filter"))
                        .on_activate(link.callback(|_| Msg::SetClientFilter(String::new()))),
                        true,
                    )
                    .on_input(link.callback(Msg::SetClientFilter));

                Column::new()
                    .class(FlexFit)
                    .with_child(
                        Panel::new().border(true).title(tr!("VLANs")).with_child(
                            DataTable::new(self.vlan_columns.clone(), self.vlans.clone())
                                .class(FlexFit),
                        ),
                    )
                    .with_child(
                        Panel::new()
                            .border(true)
                            .title(tr!("Switch Ports"))
                            .with_child(
                                DataTable::new(self.port_columns.clone(), self.ports.clone())
                                    .class(FlexFit),
                            ),
                    )
                    .with_child(
                        Panel::new()
                            .border(true)
                            .title(tr!("Clients"))
                            .with_tool(filter)
                            .with_child(
                                DataTable::new(self.client_columns.clone(), self.clients.clone())
                                    .class(FlexFit),
                            ),
                    )
                    .with_child(
                        Panel::new().border(true).title(tr!("Devices")).with_child(
                            DataTable::new(self.device_columns.clone(), self.devices.clone())
                                .class(FlexFit),
                        ),
                    )
                    .children(
                        devices_view
                            .warnings
                            .iter()
                            .map(|warning| error_message(warning).into()),
                    )
                    .with_child(
                        Container::new()
                            .padding_top(4)
                            .opacity(80)
                            .with_child(format!(
                                "{} — {} clients",
                                config.url.clone().unwrap_or_default(),
                                clients.len()
                            )),
                    )
                    .into()
            }
            (Some(_), Some(Err(_))) => Column::new()
                .with_child(
                    Container::new()
                        .padding_top(4)
                        .with_child(tr!("Configured controller is currently unreachable.")),
                )
                .into(),
            _ => Column::new()
                .with_child(
                    Container::new().style("position", "relative").with_child(
                        Progress::new()
                            .style("position", "absolute")
                            .style("left", "0")
                            .style("right", "0"),
                    ),
                )
                .with_child(
                    Container::new()
                        .padding_top(4)
                        .opacity(80)
                        .with_child(tr!("No Omada controller configured yet.")),
                )
                .with_child(
                    Container::new()
                        .padding_top(4)
                        .with_child(
                            Button::new(tr!("Configure"))
                                .class("pwt-scheme-primary")
                                .on_activate(ctx.link().change_view_callback(|_| Some(ViewState::Config))),
                        ),
                )
                .into(),
        };

        Column::new()
            .class(FlexFit)
            .with_optional_child(error)
            .with_child(body)
            .into()
    }

    fn dialog_view(
        &self,
        ctx: &LoadableComponentContext<Self>,
        _view_state: &Self::ViewState,
    ) -> Option<Html> {
        Some(
            EditWindow::new(tr!("Omada Controller"))
                .width(600)
                .submit_text(tr!("Save"))
                // Prefill the known values and send the config digest back so
                // the server's lost-update check works.
                .loader((load_omada_config, "/omada/config".to_string()))
                .submit_digest(true)
                .renderer(config_input_panel)
                .on_submit(|form_ctx: FormContext| async move {
                    let data = form_ctx.get_submit_data();
                    http_put::<bool>("/omada/config", Some(data)).await?;
                    Ok(())
                })
                .on_done(ctx.link().callback(|_| Msg::Reload))
                .into(),
        )
    }
}

/// EditWindow form loader for the Omada config (prefills url / client id /
/// omadacId and provides the digest).
async fn load_omada_config(url: yew::AttrValue) -> Result<ApiResponseData<serde_json::Value>, Error> {
    http_get_full(&*url, None).await
}

fn config_input_panel(_form_ctx: &FormContext) -> Html {
    InputPanel::new()
        .padding(4)
        .min_width(500)
        .with_field(
            tr!("URL"),
            Field::new()
                .name("url")
                .placeholder("https://172.31.12.45:8043")
                .required(true),
        )
        .with_field(
            tr!("Client ID"),
            Field::new().name("client_id").required(true),
        )
        .with_field(
            tr!("Client Secret"),
            Field::new()
                .name("client_secret")
                .placeholder(tr!("Unchanged"))
                .input_type(InputType::Password)
                .required(false),
        )
        .with_field(
            tr!("Controller ID (omadacId)"),
            Field::new()
                .name("omadac_id")
                .placeholder(tr!("Resolved automatically when absent")),
        )
        .into()
}

async fn load_omada_data() -> Result<(OmadaConfigView, Option<Result<OmadaData, String>>), Error> {
    let config: OmadaConfigView = http_get("/omada/config", None).await?;
    let data = match &config.url {
        Some(_) => Some(
            try_join!(
                http_get::<Vec<OmadaVlan>>("/omada/vlans", None),
                http_get::<OmadaDevicesView>("/omada/devices", None),
                http_get::<Vec<OmadaClientEntry>>("/omada/clients", None)
            )
            .map_err(|err| err.to_string()),
        ),
        None => None,
    };
    Ok((config, data))
}

fn filtered_clients(clients: &[OmadaClientEntry], filter: &str) -> Vec<OmadaClientEntry> {
    let filter = filter.trim().to_lowercase();
    if filter.is_empty() {
        return clients.to_vec();
    }
    clients
        .iter()
        .filter(|entry| {
            entry
                .client
                .ip
                .as_deref()
                .map(|value| value.to_lowercase().contains(&filter))
                .unwrap_or(false)
                || entry
                    .client
                    .mac
                    .as_deref()
                    .map(|value| value.to_lowercase().contains(&filter))
                    .unwrap_or(false)
                || entry
                    .client
                    .hostname
                    .as_deref()
                    .map(|value| value.to_lowercase().contains(&filter))
                    .unwrap_or(false)
                || entry
                    .client
                    .name
                    .as_deref()
                    .map(|value| value.to_lowercase().contains(&filter))
                    .unwrap_or(false)
        })
        .cloned()
        .collect()
}

fn vlan_columns(client_count: Rc<HashMap<u32, usize>>) -> Rc<Vec<DataTableHeader<OmadaVlan>>> {
    let count_sort = client_count.clone();
    let count_render = client_count;
    Rc::new(vec![
        DataTableColumn::new(tr!("Name"))
            .flex(2)
            .sorter(|a: &OmadaVlan, b: &OmadaVlan| a.name.cmp(&b.name))
            .render(|vlan: &OmadaVlan| vlan.name.clone().unwrap_or_default().into())
            .into(),
        DataTableColumn::new(tr!("VLAN ID"))
            .width("90px")
            .sorter(|a: &OmadaVlan, b: &OmadaVlan| a.vlan.cmp(&b.vlan))
            .sort_order(true)
            .render(|vlan: &OmadaVlan| vlan.vlan.map(|v| v.to_string()).unwrap_or_default().into())
            .into(),
        DataTableColumn::new(tr!("Subnet/Gateway"))
            .flex(2)
            .render(|vlan: &OmadaVlan| vlan.gateway_subnet.clone().unwrap_or_default().into())
            .into(),
        DataTableColumn::new(tr!("DHCP"))
            .width("80px")
            .render(|vlan: &OmadaVlan| match vlan.dhcp() {
                Some(true) => tr!("yes").into(),
                Some(false) => tr!("no").into(),
                None => "—".into(),
            })
            .into(),
        DataTableColumn::new(tr!("Clients"))
            .width("90px")
            .sorter(move |a: &OmadaVlan, b: &OmadaVlan| {
                count_sort
                    .get(&a.vlan.unwrap_or(0))
                    .cmp(&count_sort.get(&b.vlan.unwrap_or(0)))
            })
            .render(move |vlan: &OmadaVlan| {
                count_render
                    .get(&vlan.vlan.unwrap_or(0))
                    .map(|count| count.to_string())
                    .unwrap_or_default()
                    .into()
            })
            .into(),
    ])
}

fn port_columns() -> Rc<Vec<DataTableHeader<PortRow>>> {
    Rc::new(vec![
        DataTableColumn::new(tr!("Switch"))
            .flex(2)
            .sorter(|a: &PortRow, b: &PortRow| a.switch_name.cmp(&b.switch_name))
            .render(|row: &PortRow| format!("{} ({})", row.switch_name, row.switch_mac).into())
            .into(),
        DataTableColumn::new(tr!("Port"))
            .width("70px")
            .sorter(|a: &PortRow, b: &PortRow| a.port_index.cmp(&b.port_index))
            .sort_order(true)
            .render(|row: &PortRow| row.port_index.to_string().into())
            .into(),
        DataTableColumn::new(tr!("Name"))
            .flex(1)
            .render(|row: &PortRow| row.port_name.clone().into())
            .into(),
        DataTableColumn::new(tr!("Profile"))
            .flex(1)
            .render(|row: &PortRow| row.profile_name.clone().into())
            .into(),
        DataTableColumn::new(tr!("PoE"))
            .width("80px")
            .render(|row: &PortRow| row.poe_mode.clone().into())
            .into(),
        DataTableColumn::new(tr!("Client"))
            .flex(2)
            .render(|row: &PortRow| {
                row.client
                    .as_ref()
                    .map(|client| {
                        format!(
                            "{} ({})",
                            client.client.hostname.clone().unwrap_or_else(|| client
                                .client
                                .name
                                .clone()
                                .unwrap_or_default()),
                            client.client.ip.clone().unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| "—".to_string())
                    .into()
            })
            .into(),
        DataTableColumn::new(tr!("PDM Remote"))
            .flex(1)
            .render(|row: &PortRow| pve_badge(row.client.as_ref().and_then(|c| c.pve.as_ref())))
            .into(),
    ])
}

fn client_columns(
    vlan_names: Rc<HashMap<u32, String>>,
) -> Rc<Vec<DataTableHeader<OmadaClientEntry>>> {
    Rc::new(vec![
        DataTableColumn::new(tr!("IP"))
            .flex(1)
            .sorter(|a: &OmadaClientEntry, b: &OmadaClientEntry| a.client.ip.cmp(&b.client.ip))
            .render(|entry: &OmadaClientEntry| entry.client.ip.clone().unwrap_or_default().into())
            .into(),
        DataTableColumn::new(tr!("MAC"))
            .width("150px")
            .render(|entry: &OmadaClientEntry| entry.client.mac.clone().unwrap_or_default().into())
            .into(),
        DataTableColumn::new(tr!("Hostname"))
            .flex(1)
            .render(|entry: &OmadaClientEntry| {
                entry
                    .client
                    .hostname
                    .clone()
                    .unwrap_or_else(|| entry.client.name.clone().unwrap_or_default())
                    .into()
            })
            .into(),
        DataTableColumn::new(tr!("VLAN"))
            .width("80px")
            .sorter(|a: &OmadaClientEntry, b: &OmadaClientEntry| a.client.vid.cmp(&b.client.vid))
            .render(move |entry: &OmadaClientEntry| {
                entry
                    .client
                    .vid
                    .map(|vid| {
                        vlan_names
                            .get(&vid)
                            .cloned()
                            .unwrap_or_else(|| vid.to_string())
                    })
                    .unwrap_or_default()
                    .into()
            })
            .into(),
        DataTableColumn::new(tr!("Location"))
            .flex(2)
            .render(|entry: &OmadaClientEntry| client_location(&entry.client).into())
            .into(),
        DataTableColumn::new(tr!("Status"))
            .width("110px")
            .render(|entry: &OmadaClientEntry| client_status(&entry.client).into())
            .into(),
        DataTableColumn::new(tr!("PDM Remote"))
            .flex(1)
            .render(|entry: &OmadaClientEntry| pve_badge(entry.pve.as_ref()))
            .into(),
    ])
}

fn device_columns() -> Rc<Vec<DataTableHeader<OmadaDevice>>> {
    Rc::new(vec![
        DataTableColumn::new(tr!("Name"))
            .flex(2)
            .sorter(|a: &OmadaDevice, b: &OmadaDevice| a.name.cmp(&b.name))
            .render(|device: &OmadaDevice| device.name.clone().unwrap_or_default().into())
            .into(),
        DataTableColumn::new(tr!("Type"))
            .width("90px")
            .render(|device: &OmadaDevice| device.device_type.clone().unwrap_or_default().into())
            .into(),
        DataTableColumn::new(tr!("IP"))
            .width("140px")
            .render(|device: &OmadaDevice| device.ip.clone().unwrap_or_default().into())
            .into(),
        DataTableColumn::new(tr!("Firmware"))
            .width("120px")
            .render(|device: &OmadaDevice| device.firmware.clone().unwrap_or_default().into())
            .into(),
        DataTableColumn::new(tr!("Status"))
            .width("90px")
            .render(|device: &OmadaDevice| {
                device
                    .status
                    .map(|status| status.to_string())
                    .unwrap_or_default()
                    .into()
            })
            .into(),
    ])
}

fn pve_badge(pve: Option<&PveMatch>) -> Html {
    match pve {
        Some(pve) => Badge::new(format!(
            "{}/{}",
            pve.remote,
            pve.node.as_deref().unwrap_or("—")
        ))
        .into(),
        None => Html::default(),
    }
}

fn client_location(client: &OmadaClient) -> String {
    if let Some(switch_name) = &client.switch_name {
        if let Some(port) = client.port {
            return format!("{switch_name} port {port}");
        }
        return switch_name.clone();
    }
    if let Some(ap_name) = &client.ap_name {
        return ap_name.clone();
    }
    if let Some(port) = client.port {
        return format!("gateway port {port}");
    }
    "—".to_string()
}

fn client_status(client: &OmadaClient) -> String {
    let mut status = match client.active {
        Some(true) => tr!("online"),
        Some(false) => tr!("offline"),
        None => "—".to_string(),
    };
    if client.blocked == Some(true) {
        status = format!("{status} (blocked)");
    }
    status
}
