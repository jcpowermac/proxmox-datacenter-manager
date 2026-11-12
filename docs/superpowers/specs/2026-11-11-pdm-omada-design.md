# Design: Omada network section in Proxmox Datacenter Manager (read-only phase 1)

Date: 2026-11-11
Status: draft, awaiting owner approval
Fork: `jcpowermac/proxmox-datacenter-manager` @ upstream master `aec1088` (v1.1.9)

## Intent and why

The homelab runs three standalone Proxmox VE 9 nodes, centrally managed by PDM
(LXC 103, `172.31.12.95:8443`), and its network (ER7206 gateway, SG3428 switch,
three EAPs) is managed by a TP-Link Omada software controller v6.3.0.111
(LXC, `172.31.12.45:8043`). Today that means two UIs for one estate.

Goal: PDM becomes the single pane of glass. A new read-only "Omada" section in
PDM shows:

1. **VLANs** (Omada LAN networks: name, VLAN ID, subnet/gateway, DHCP, device counts)
2. **Physical switch ports** — full port table of the SG3428, with which VLAN
   profile each port carries and which client sits on it; the three PVE
   management ports highlighted (verified live: `172.31.12.97`→port 17,
   `172.31.12.98`→port 4, `172.31.12.99`→port 8, all VLAN 12 "servers")
3. **IP management** — client table: IP, MAC, hostname, VLAN, switch port or
   AP, online/offline, blocked; with PDM's PVE remotes annotated on the
   matching clients (join on the node management IPs from `remotes.cfg`)

Everything behind PDM's existing TLS + authentication: one login, no new
public ports, no Omada credentials in the browser.

## Goals

- Read-only display phase; writes (block client, port reconfig, add VLAN) are
  explicitly phase 2.
- One Omada controller + one site (lab reality), but the API takes a site
  parameter so a second site doesn't break anything.
- Config stored in PDM's config dir with the secret in a shadow file,
  editable through the PDM API (GET/PUT) and a small UI form.
- Additive to upstream: new modules plus ~2-line registrations, so rebasing
  future PDM releases stays cheap.

## Non-goals (phase 1)

- Write actions of any kind (except the PDM-side config PUT).
- AP/Wi-Fi detail views, traffic counters/RRD history, alarms, cable test.
- Omada objects appearing in the PDM resource tree, global search, or
  dashboards/"views" (they stay in their own section).
- A CLI surface (API is enough; the generic PDM client can hit it).
- Multi-controller support.

## Anti-goals

- Do **not** add `RemoteType::Omada` — that enum is matched in ~25 places
  (fetchers, task cache, updates, ceph) and would drag the onboarding wizard
  into it.
- Do **not** use Omada's internal UI protocol (`/{omadacId}/api/v2`); use the
  documented OpenAPI v1 surface even though it has quirks (below).
- Do **not** fetch Omada directly from the browser UI (credentials + CORS).

## Constraints

- Fork of PDM v1.1.9, AGPL-3. Rust 2021 workspace; UI is Yew 0.21 → WASM
  (Trunk, `proxmox-wasm-builder`), Proxmox `pwt` widget toolkit.
- Build = Proxmox deb pipeline; install custom `.deb` over stock in LXC 103.
  This is the main one-time cost/schedule risk.
- Omada controller uses self-signed TLS; the PDM server's HTTP client must
  allow it the same way `server/src/connection.rs` does for PVE clients.
- Single-user lab today (root@pam); keep the permission model simple.

## Approaches considered

- **A. New top-level API section + UI module (chosen).** `server/src/api/omada/`
  proxy over Omada OpenAPI v1 + config section + `ui/src/omada/` Yew views.
  Additive, small blast radius, sits behind PDM auth. Cost: one-off UI work.
- **B. `RemoteType::Omada`.** Cleaner conceptually ("remote = managed
  system") but touches every match arm, the onboarding wizard, task/update
  machinery that assumes PVE/PBS shapes; Omada's single-site model fits
  "remote + nodes" awkwardly. Defer until A proves out.
- **C. Fake-PVE-remote shim.** Zero PDM source changes (sidecar that serves a
  fake `/cluster/resources` encoding VLAN/port data). Cheapest start but
  wrong semantics (VLANs as "guests"), awkward writes, and RRD/subscription
  machinery chokes on non-PVE data. Kept only as a spike fallback.

## Verified Omada OpenAPI v1 facts

Probed against the live controller (v6.3.0.111) on 2026-11-11:

- `GET /api/info` (no auth) → `result.omadacId`.
- `POST /openapi/authorize/token?grant_type=client_credentials` body
  `{omadacId, client_id, client_secret}` → `{accessToken, expiresIn: 7200}`.
  Header for all calls: `Authorization: AccessToken=<token>`
  (**not** `Bearer`; Bearer → 401 `-44116`).
- List endpoints **require page params** or Tomcat returns a raw 400:
  `?page=N&pageSize=M` (not `pageNo`).
- Sites: `GET /openapi/v1/{om}/sites?page=1&pageSize=50`.
- LAN networks (VLANs): `GET /openapi/v1/{om}/sites/{sid}/lan-networks?page=1&pageSize=100`
  → fields incl. `id`, `name`, `vlan` (numeric VLAN ID), `dhcpSettingsVO`,
  `gatewaySubnet`, `vlanType`. Lab has 8 (legacy 1, people-devices 30,
  servers 12, camera 66, minecraft 45, management 100, iot 1066, work 666).
- Devices: `GET /openapi/v1/{om}/sites/{sid}/devices` → all device types in
  one list with `type` (`gateway`/`switch`/`ap`), `mac`, `ip`, firmware,
  `status`. The `deviceType` query param appears ignored on 6.3.0.111 —
  filter client-side.
- Switch detail + **all physical ports**: `GET /openapi/v1/{om}/sites/{sid}/switches/{mac}`
  → device info + `portList[]` (28 ports on the SG3428).
  **Port record is thin**: only `port`, `name`, `profileId`, `profileName`,
  `profileOverrideEnable`, `poeMode`, `lagPort`, `status` (all 28 report
  `1`). **No PVID, no tagged-VLAN list, no link speed/state.**
  Probed and unavailable on this controller: `port-profiles`,
  `switch-port-profiles`, `switches/{mac}/port-profiles` → 404;
  `switches/ports/poe-info` → `errorCode 0` but **0 rows**. So per-port VLAN
  membership is *not* reachable over OpenAPI v1 — see the Switch-ports view
  below for what is shown instead.
- Clients: `GET /openapi/v1/{om}/sites/{sid}/clients?page=N&pageSize=100`,
  paginate until `totalRows` reached (lab: 61 clients). Record fields include
  an `result.totalRows` total (not `total`), `ip`, `mac`, `hostName`, `name`,
  `vid` (VLAN ID), `port` (switch port
  number for wired), `switchMac` + `switchName` (present on wired clients
  connected to Omada-managed switches; verified live: 24 of 29 wired lab
  clients carry it, e.g. pve2 → `28-87-BA-61-24-DE`; wired clients without
  it sit on the ER7206 gateway's own ports), `connectType` (1 = wired /
  switch, 2 = wireless/AP per the captured fixtures; the code keys off
  `switchMac`/`apName` rather than this field),
  `networkName`, `apName`, `blocked`, `active`, `lastSeen`.
- PVE-node join: PVE management IPs appear as clients (verified `.97`, `.98`,
  `.99` are all present with port/vid). **But `remotes.cfg` may not hold
  IPs** — its `nodes` field is `Vec<PropertyString<NodeUrl>>` and the API
  schema accepts "a cluster node IP *or* hostname", as entered at onboarding.
  The lab also has two nodes both named `pve` (.97, .99), so name matching is
  ambiguous. Build the match set per remote as {configured host} ∪ {DNS A
  records resolved from that host} ∪ {node name}, then match Omada client
  IPs against it; cache alongside the token. Verify the actual
  `remotes.cfg` node form before implementing the join. Mapping listed in
  the intent section.
- **Port join**: clients → physical ports is done by `switchMac` (client
  record) → switch device, then `port`. Future-proof to a second switch and
  correct for gateway-ported clients (no `switchMac` = ER7206 port, rendered
  as "gateway port N").
- **Site parameter**: all site-scoped GETs take an optional `?site=<siteId>`;
  default = the sole site resolved from `GET /sites` (cached with the token).
  No `site` entry in the config in phase 1.
- Controller drops connections under bursty load (see lab `omada/NOTES.md`):
  the Omada HTTP client needs retry-with-backoff, not just token caching.
- Errors: `errorCode != 0` with `msg` (e.g. `-44116` bad auth,
  `-7131` bad omadacId, `-39050` device not found). Treat non-zero as error
  and surface `msg`.

## Architecture

### Config — `lib/pdm-config/src/omada.rs` (new module)

Single-controller config in the PDM config dir, following the
`remotes.rs` pattern: section-style file (e.g. `omada.cfg`) with
`url`, `omadac-id` (optional: auto-resolve via `/api/info`), `client-id`,
plus a shadow file (e.g. `omada.shadow`) holding `client-secret`, modeled on
`remotes.shadow` handling. `init()` registered in
`server/src/context/mod.rs` next to `pdm_config::remotes::init`.

### Server — `server/src/api/omada/` (new module)

Registered in `server/src/api/mod.rs` SUBDIRS: `("omada", &omada::ROUTER)`.

Methods (phase 1):

| Method | Path | Purpose |
|---|---|---|
| GET/PUT | `/omada/config` | Read/update the controller config. **GET never returns `client-secret`** (renders as unset). PUT with an absent/empty secret keeps the stored one; PUT carries a `ConfigDigest` for lost-update protection, like `config/notes` |
| GET | `/omada/vlans` | LAN networks (VLANs) |
| GET | `/omada/devices` | Site devices (gateway/switch/APs), incl. switch `portList` for switches |
| GET | `/omada/clients` | Clients (optional `?site=`); server fetches all pages (cap, e.g. 2000) and **annotates** each record with PDM info when its IP matches a PVE remote node IP (remote id + node name) |

`/omada/vlans` and `/omada/devices` also accept the optional `?site=` param.

Server-side pieces:

- Omada client: reqwest with self-signed-TLS tolerance (pattern:
  `server/src/connection.rs`), token cached with expiry and refreshed
  ~5 min before it lapses; single-flight (tokio Mutex) so concurrent API
  calls don't stampede the token endpoint; retry-with-backoff on
  connection-level failures (the controller drops connections under bursty
  load).
- Serde response types in `lib/pdm-api-types/src/omada.rs` (new): map the
  **subset** of fields the UI needs; `#[serde(default)]` + ignore unknowns
  so Omada schema drift doesn't break parsing.
- Permissions: reads = `Permission::Privilege(&["system"], PRIV_SYS_AUDIT,
  false)` (same as Notes); config PUT = `PRIV_SYS_MODIFY`.
- `/omada/devices` fans out one `switches/{mac}` call per switch to get
  `portList` (N+1, fine for one switch, note it).
- Optional short-TTL memo cache (~10-15 s) on the upstream reads: the UI
  auto-refreshes and the controller drops connections under bursty load.
- Config GET/PUT endpoints in the same module, guarded as above; secret
  write-only on PUT unless explicitly re-sent.

### UI — `ui/src/omada/` (new module)

- Top-level menu item "Omada" in `PdmMainMenu` (`ui/src/main_menu.rs`),
  registered exactly like Notes (no route change needed; menu key based),
  gated on `["system"]` `PRIV_SYS_AUDIT`.
- One view, three stacked/tabbed tables:
  - **VLANs** — name, VLAN ID, subnet/gateway, DHCP on/off, (device counts
    computed in the browser from the clients list)
  - **Switch ports** — one row per physical port: port number, name, port
    **profile name** (the only VLAN signal v1 exposes; the profile's VLAN
    list is not available per port), PoE mode, and the **client on that port**
    (hostname/IP, resolved from the clients join); **PVE nodes visually
    highlighted** (badge with the PDM remote/node id). Explicitly do not
    promise PVID/tagged VLANs or link speed — v1 does not expose them
    (verified above).
  - **Clients** — IP, MAC, hostname, VLAN (resolved name via the VLAN
    list), switch port / AP, online/offline, blocked; client-side search
    box; PVE-node badge as above
- Data via `proxmox_yew_comp::http_get("/omada/...")`; auto-refresh ~60 s
  (builder call). Empty state when no controller configured, linking to a
  small config form (GET/PUT `/omada/config`).

### Data flow

```
UI (Yew) ──/api2/json/omada/*──▶ PDM server ──token cache──▶ Omada controller
  ▲                                (reqwest, self-signed ok)      /openapi/v1/{om}/sites/{sid}/…
  └──────────────────────────── JSON ─────────────────────────────┘
```

## Left to the builder

- Exact serde field subset and the error mapping (Omada `errorCode`/`msg` →
  PDM API error format).
- Client pagination strategy details (cap value, failure mode on a page
  error: return partial with a warning field or fail — recommend fail, it's
  read-only data).
- Config file format details (follow `remotes.cfg`/shadow conventions).
- Unit tests: token cache/expiry, client→PVE-remote join logic against
  fixture JSON; integration test wiring follows `server/tests` style.
- Whether to also render the gateway/AP device list as a small extra table
  (recommended yes — the `devices` endpoint is already fetched).
- **Open for owner (Q3)**: does this engagement end at "custom `.deb`
  builds + integration tests pass", or at "installed over stock PDM in
  LXC 103, Omada section verified live, homelab `docs/` updated in the same
  commit"? The plan's deployment/validation tail depends on the answer.
- Auto-refresh interval and any light caching in the UI.

## Risks

- **Fork maintenance**: rebase on each PDM release. Mitigation: keep the
  change additive (new files + a handful of registration lines); avoid
  touching shared code paths.
- **Undocumented Omada surface**: page-param 400s, ignored `deviceType`,
  endpoint drift between controller versions. Mitigation: pin mappings to
  the observed behavior above, permissive deserialization, and a probe
  script (`omada/omada-probe/` in the homelab repo) to re-verify before
  deploying a controller upgrade.
- **Deb build environment**: one-time yak-shave (Proxmox apt deps,
  `proxmox-wasm-builder`); budget for it in the plan.
- **Package upgrade / rollback**: this replaces a distro package on a live
  service. Build a fork-versioned package (e.g. `1.1.9+omada1`) so apt never
  advertises an upgrade over it, `apt-mark hold` the package in LXC 103, keep
  the stock `.deb` for rollback, and record the rollback command. Budget
  decision for the owner under Q3.

## Post-deploy (homelab repo)

Per homelab AGENTS.md, update `docs/` in the same commit as the change that
reveals it: PDM now also provides network visibility; controller is actually
v6.3.0.111 (docs say 6.3.0.45) and **is** cloud-registered (docs say not) —
both discovered during this probe.
