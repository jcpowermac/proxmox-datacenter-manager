//! Omada OpenAPI v1 client: token cache, transport retry, pagination, error mapping.
//!
//! Wire behavior is pinned by the spec's "Verified Omada OpenAPI v1 facts"
//! (docs/superpowers/specs/2026-11-11-pdm-omada-design.md).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Error, bail};
use async_trait::async_trait;
use http_body_util::BodyExt;
use pdm_api_types::omada::OmadaApiClient;

pub const PAGE_SIZE: u32 = 100;
/// Hard cap on collected items per list endpoint.
pub const MAX_ITEMS: usize = 2000;
/// Refresh the token this long before it expires.
pub const REFRESH_WINDOW: Duration = Duration::from_secs(300);
/// Per-request timeout for the real transport.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A cached access token with its expiry.
#[derive(Clone, Debug)]
pub struct TokenEntry {
    pub token: String,
    pub expires_at: Instant,
}

/// True when the token must be re-fetched (expired or within the refresh window).
pub fn should_refresh(entry: &TokenEntry, now: Instant) -> bool {
    entry.expires_at <= now + REFRESH_WINDOW
}

/// Transport abstraction so the client is testable without a real controller.
#[async_trait]
pub trait OmadaTransport: Send + Sync {
    /// Perform one HTTP request; returns the response body as a string.
    /// Failures that happen *before a full body was received* (connection,
    /// timeout) must carry a [`TransportFailure`] in the error chain so
    /// [`is_transport_error`] can classify them.
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Result<String, Error>;
}

/// Marker for failures before a full response body was received (connection
/// refused/reset, timeout). Safe to retry.
#[derive(Debug, thiserror::Error)]
#[error("transport error")]
pub struct TransportFailure;

/// True when the error is a transport-level failure (retryable), as opposed to
/// an API-level error (e.g. an `Omada error -44116: ...` envelope error, which
/// means a full body was received).
pub fn is_transport_error(err: &Error) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<TransportFailure>().is_some())
}

/// The real transport: a hyper client with an accept-any-certificate TLS
/// callback (Omada controllers ship self-signed certs; mirrors
/// `probe_tls_connection`, server/src/connection.rs).
///
/// `proxmox_http::client::Client` is used instead of `proxmox_client::Client`
/// because the latter's token auth emits PVE-style `Authorization` headers
/// (`prefix:userid!value`) that do not match Omada's `AccessToken=<token>`
/// format, and its non-2xx handling consumes the body we need for envelope
/// errors.
struct ProxmoxTransport {
    client: proxmox_http::client::Client,
}

impl ProxmoxTransport {
    fn new() -> Result<Self, Error> {
        let mut connector = openssl::ssl::SslConnector::builder(openssl::ssl::SslMethod::tls())?;
        connector.set_verify_callback(openssl::ssl::SslVerifyMode::PEER, |_valid, _details| true);
        Ok(Self {
            client: proxmox_http::client::Client::with_ssl_connector(
                connector.build(),
                Default::default(),
            ),
        })
    }
}

#[async_trait]
impl OmadaTransport for ProxmoxTransport {
    async fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Result<String, Error> {
        let mut builder = http::request::Request::builder().method(method).uri(url);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let http_body: proxmox_http::Body = body.map(str::to_string).unwrap_or_default().into();
        let request = builder
            .body(http_body)
            .map_err(|e| Error::msg(format!("building request for {url} failed: {e}")))?;

        // no success status -> still return the body (Omada reports errors as
        // JSON envelopes with HTTP 200; a raw 400 body is surfaced as a
        // non-transport parse error by the caller)
        let response = match tokio::time::timeout(REQUEST_TIMEOUT, self.client.request(request))
            .await
        {
            Err(_) => Err(anyhow::anyhow!(TransportFailure).context(format!(
                "request to {url} timed out after {REQUEST_TIMEOUT:?}"
            ))),
            Ok(result) => result.map_err(|e| {
                anyhow::anyhow!(TransportFailure).context(format!("request to {url} failed: {e}"))
            }),
        }?;

        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| Error::new(TransportFailure).context(e))?;
        String::from_utf8(body.to_bytes().to_vec())
            .map_err(|e| Error::msg(format!("response from {url} is not utf-8: {e}")))
    }
}

/// Envelope for list endpoints: `result.items` + `result.totalRows`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OmadaResult<T> {
    #[serde(default)]
    total_rows: u32,
    #[serde(default)]
    items: Vec<T>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OmadaEnvelope<T> {
    #[serde(default)]
    error_code: i32,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    result: Option<OmadaResult<T>>,
}

/// Envelope where `result` is an opaque object (`/api/info`, switch detail,
/// token).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct OmadaEnvelopeRaw {
    #[serde(default)]
    error_code: i32,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    result: Option<serde_json::Value>,
}

fn check_error_code(error_code: i32, msg: Option<&str>) -> Result<(), Error> {
    if error_code != 0 {
        bail!("Omada error {error_code}: {}", msg.unwrap_or_default());
    }
    Ok(())
}

impl OmadaHttpClient {
    pub fn new(remote: &pdm_config::omada::OmadaRemote) -> Result<Self, Error> {
        Ok(Self {
            base: remote.url.trim_end_matches('/').to_string(),
            client_id: remote.client_id.clone(),
            // a missing secret surfaces as an Omada auth error at request time
            client_secret: remote.client_secret.clone().unwrap_or_default(),
            omadac_id: tokio::sync::Mutex::new(remote.omadac_id.clone()),
            token: tokio::sync::Mutex::new(None),
            site: tokio::sync::Mutex::new(None),
            transport: Arc::new(ProxmoxTransport::new()?),
        })
    }

    /// The controller id, cached after the first `GET /api/info`.
    async fn omadac_id(&self) -> Result<String, Error> {
        if let Some(id) = self.omadac_id.lock().await.clone() {
            return Ok(id);
        }
        let body = self
            .request_with_retry("GET", &format!("{}/api/info", self.base), &[], None)
            .await?;
        let env: OmadaEnvelopeRaw = serde_json::from_str(&body)
            .map_err(|e| Error::msg(format!("parsing /api/info response failed: {e}")))?;
        check_error_code(env.error_code, env.msg.as_deref())?;
        let id = env
            .result
            .and_then(|r| {
                r.get("omadacId")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .ok_or_else(|| Error::msg("/api/info response has no result.omadacId"))?;
        *self.omadac_id.lock().await = Some(id.clone());
        Ok(id)
    }

    /// A valid access token; single-flight via the mutex (held across
    /// check -> fetch -> store).
    async fn token(&self) -> Result<String, Error> {
        let mut guard = self.token.lock().await;
        let now = Instant::now();
        if let Some(entry) = guard.as_ref() {
            if !should_refresh(entry, now) {
                return Ok(entry.token.clone());
            }
        }
        let om = self.omadac_id().await?;
        let body = serde_json::json!({
            "omadacId": om,
            "client_id": self.client_id,
            "client_secret": self.client_secret,
        })
        .to_string();
        let resp = self
            .request_with_retry(
                "POST",
                &format!(
                    "{}/openapi/authorize/token?grant_type=client_credentials",
                    self.base
                ),
                &[("Content-Type", "application/json")],
                Some(&body),
            )
            .await?;

        // the controller wraps the token in the standard envelope; accept a
        // flat {accessToken, expiresIn} body too, in case of drift
        let value: serde_json::Value = serde_json::from_str(&resp)
            .map_err(|e| Error::msg(format!("parsing token response failed: {e}")))?;
        let inner = value
            .get("result")
            .filter(|r| r.is_object())
            .unwrap_or(&value);
        let token = inner
            .get("accessToken")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                // surface the controller's error (e.g. -44116 bad credentials)
                // instead of a generic "no accessToken" message
                match value
                    .get("errorCode")
                    .and_then(|e| e.as_i64())
                    .filter(|e| *e != 0)
                {
                    Some(code) => Error::msg(format!(
                        "Omada authorize failed with errorCode {code}: {}",
                        value
                            .get("msg")
                            .and_then(|m| m.as_str())
                            .unwrap_or("(no message)")
                    )),
                    None => Error::msg("token response has no accessToken"),
                }
            })?;
        let expires_in: u64 = inner
            .get("expiresIn")
            .and_then(|v| v.as_u64())
            .unwrap_or(7200);
        let entry = TokenEntry {
            token: token.to_string(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        };
        *guard = Some(entry.clone());
        Ok(entry.token)
    }

    /// The site id to use: the override, or the default (first site, cached).
    async fn site_id(&self, site: Option<&str>) -> Result<String, Error> {
        if let Some(site) = site {
            return Ok(site.to_string());
        }
        let mut guard = self.site.lock().await;
        if let Some(site) = guard.as_ref() {
            return Ok(site.clone());
        }
        let om = self.omadac_id().await?;
        let token = self.token().await?;
        let body = self
            .request_with_retry(
                "GET",
                &format!("{}/openapi/v1/{om}/sites?page=1&pageSize=50", self.base),
                &[("Authorization", &format!("AccessToken={token}"))],
                None,
            )
            .await?;
        let env: OmadaEnvelope<pdm_api_types::omada::OmadaSite> = serde_json::from_str(&body)
            .map_err(|e| Error::msg(format!("parsing sites response failed: {e}")))?;
        check_error_code(env.error_code, env.msg.as_deref())?;
        let id = env
            .result
            .and_then(|r| r.items.into_iter().next())
            .and_then(|s| s.id)
            .ok_or_else(|| Error::msg("controller has no sites"))?;
        *guard = Some(id.clone());
        Ok(id)
    }

    /// One transport request with retry on transport-level failures only
    /// (up to 3 attempts; 200 ms then 1 s delay). Never retries after a full
    /// body was received.
    async fn request_with_retry(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Result<String, Error> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.transport.request(method, url, headers, body).await {
                Ok(body) => return Ok(body),
                Err(err) if is_transport_error(&err) && attempt < 3 => {
                    let delay = if attempt == 1 {
                        Duration::from_millis(200)
                    } else {
                        Duration::from_secs(1)
                    };
                    tokio::time::sleep(delay).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Fetch all pages of a list endpoint.
    async fn fetch_all<T: serde::de::DeserializeOwned + Default>(
        &self,
        path: String,
        token: &str,
    ) -> Result<Vec<T>, Error> {
        let mut collected: Vec<T> = Vec::new();
        let mut page = 1u32;
        loop {
            let body = self
                .request_with_retry(
                    "GET",
                    &format!("{path}?page={page}&pageSize={PAGE_SIZE}"),
                    &[("Authorization", &format!("AccessToken={token}"))],
                    None,
                )
                .await?;
            let env: OmadaEnvelope<T> = serde_json::from_str(&body)
                .map_err(|e| Error::msg(format!("parsing {path} response failed: {e}")))?;
            check_error_code(env.error_code, env.msg.as_deref())?;
            let result = env
                .result
                .ok_or_else(|| Error::msg(format!("{path} response has no result")))?;
            let total_rows = result.total_rows as usize;
            let page_len = result.items.len();
            collected.extend(result.items);
            if page_len == 0 || collected.len() >= total_rows || collected.len() >= MAX_ITEMS {
                if collected.len() >= MAX_ITEMS && collected.len() < total_rows {
                    log::warn!(
                        "Omada list {path} truncated at {MAX_ITEMS} items (totalRows {total_rows})"
                    );
                }
                break;
            }
            page += 1;
        }
        Ok(collected)
    }
}

/// The Omada client: owns the transport plus cached omadacId, token and default site.
pub struct OmadaHttpClient {
    /// Controller base URL, e.g. `https://172.31.12.45:8043` (no trailing slash).
    pub(crate) base: String,
    pub(crate) client_id: String,
    pub(crate) client_secret: String,
    /// Resolved from `GET /api/info` when the config has no explicit id.
    pub(crate) omadac_id: tokio::sync::Mutex<Option<String>>,
    pub(crate) token: tokio::sync::Mutex<Option<TokenEntry>>,
    /// Default site id, resolved from `GET /openapi/v1/{om}/sites`.
    pub(crate) site: tokio::sync::Mutex<Option<String>>,
    pub(crate) transport: Arc<dyn OmadaTransport>,
}

#[async_trait]
impl OmadaApiClient for OmadaHttpClient {
    async fn vlans(
        &self,
        site: Option<&str>,
    ) -> Result<Vec<pdm_api_types::omada::OmadaVlan>, Error> {
        let om = self.omadac_id().await?;
        let site = self.site_id(site).await?;
        let token = self.token().await?;
        let path = format!("{}/openapi/v1/{om}/sites/{site}/lan-networks", self.base);
        self.fetch_all::<pdm_api_types::omada::OmadaVlan>(path, &token)
            .await
    }

    async fn devices(
        &self,
        site: Option<&str>,
    ) -> Result<Vec<pdm_api_types::omada::OmadaDevice>, Error> {
        let om = self.omadac_id().await?;
        let site = self.site_id(site).await?;
        let token = self.token().await?;
        let path = format!("{}/openapi/v1/{om}/sites/{site}/devices", self.base);
        self.fetch_all::<pdm_api_types::omada::OmadaDevice>(path, &token)
            .await
    }

    async fn switch_ports(
        &self,
        site: Option<&str>,
        mac: &str,
    ) -> Result<Vec<pdm_api_types::omada::OmadaPort>, Error> {
        let om = self.omadac_id().await?;
        let site = self.site_id(site).await?;
        let token = self.token().await?;
        let body = self
            .request_with_retry(
                "GET",
                &format!("{}/openapi/v1/{om}/sites/{site}/switches/{mac}", self.base),
                &[("Authorization", &format!("AccessToken={token}"))],
                None,
            )
            .await?;
        let env: OmadaEnvelopeRaw = serde_json::from_str(&body)
            .map_err(|e| Error::msg(format!("parsing switch response failed: {e}")))?;
        check_error_code(env.error_code, env.msg.as_deref())?;
        let result = env
            .result
            .ok_or_else(|| Error::msg("switch response has no result"))?;
        // verified shape is `result.portList`; accept `items` if a firmware
        // revision moves the list
        let ports = result
            .get("portList")
            .or_else(|| result.get("items"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                serde_json::from_value::<Vec<pdm_api_types::omada::OmadaPort>>(
                    serde_json::Value::Array(arr.clone()),
                )
            })
            .transpose()?;
        Ok(ports.unwrap_or_default())
    }

    async fn clients(
        &self,
        site: Option<&str>,
    ) -> Result<Vec<pdm_api_types::omada::OmadaClient>, Error> {
        let om = self.omadac_id().await?;
        let site = self.site_id(site).await?;
        let token = self.token().await?;
        let path = format!("{}/openapi/v1/{om}/sites/{site}/clients", self.base);
        self.fetch_all::<pdm_api_types::omada::OmadaClient>(path, &token)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use super::*;
    use anyhow::anyhow;
    use pdm_api_types::omada::OmadaApiClient;
    use serde_json::json;
    use tokio::sync::Mutex;

    /// Canned responses, in order. Each entry is (body, status).
    struct MockTransport {
        responses: Mutex<VecDeque<(String, u32)>>,
        urls: Mutex<Vec<String>>,
        auth_calls: Mutex<u32>,
    }

    impl MockTransport {
        fn new(responses: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|r| (r, 200u32))
                        .collect::<VecDeque<_>>(),
                ),
                urls: Mutex::new(Vec::new()),
                auth_calls: Mutex::new(0),
            })
        }
    }

    #[async_trait]
    impl OmadaTransport for MockTransport {
        async fn request(
            &self,
            _method: &str,
            url: &str,
            _headers: &[(&str, &str)],
            _body: Option<&str>,
        ) -> Result<String, Error> {
            if url.contains("/openapi/authorize/token") {
                *self.auth_calls.lock().await += 1;
            }
            self.urls.lock().await.push(url.to_string());
            let (body, _status) = self
                .responses
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| anyhow!("mock transport queue empty"))?;
            Ok(body)
        }
    }

    /// A client with pre-seeded omadacId/token/site so only list pages are consumed.
    fn mock_client(responses: Vec<String>) -> (OmadaHttpClient, Arc<MockTransport>) {
        let mock = MockTransport::new(responses);
        let client = OmadaHttpClient {
            base: "https://om.test".to_string(),
            client_id: "pdm".to_string(),
            client_secret: "s3cret".to_string(),
            omadac_id: Mutex::new(Some("OM-1".to_string())),
            token: Mutex::new(Some(TokenEntry {
                token: "tok".to_string(),
                expires_at: Instant::now() + Duration::from_secs(3600),
            })),
            site: Mutex::new(Some("site-1".to_string())),
            transport: mock.clone(),
        };
        (client, mock)
    }

    /// A fresh client: no token (auth will be triggered), omadacId + site pre-seeded.
    fn fresh_client(responses: Vec<String>) -> (OmadaHttpClient, Arc<MockTransport>) {
        let mock = MockTransport::new(responses);
        let client = OmadaHttpClient {
            base: "https://om.test".to_string(),
            client_id: "pdm".to_string(),
            client_secret: "s3cret".to_string(),
            omadac_id: Mutex::new(Some("OM-1".to_string())),
            token: Mutex::new(None),
            site: Mutex::new(Some("site-1".to_string())),
            transport: mock.clone(),
        };
        (client, mock)
    }

    const AUTH_RESPONSE: &str =
        r#"{"errorCode":0,"msg":"success","result":{"accessToken":"tok2","expiresIn":7200}}"#;
    const EMPTY_PAGE: &str =
        r#"{"errorCode":0,"msg":"success","result":{"totalRows":0,"items":[]}}"#;

    fn list_page(total_rows: u32, count: u32) -> String {
        let items: Vec<_> = (0..count)
            .map(|i| json!({ "id": format!("id-{i}"), "vlan": i + 1 }))
            .collect();
        json!({
            "errorCode": 0,
            "msg": "success",
            "result": { "totalRows": total_rows, "items": items },
        })
        .to_string()
    }

    #[test]
    fn refreshes_within_5min_window() {
        let now = Instant::now();
        assert!(should_refresh(
            &TokenEntry {
                token: "t".into(),
                expires_at: now + Duration::from_secs(299)
            },
            now
        ));
        assert!(!should_refresh(
            &TokenEntry {
                token: "t".into(),
                expires_at: now + Duration::from_secs(3000)
            },
            now
        ));
        assert!(should_refresh(
            &TokenEntry {
                token: "t".into(),
                expires_at: now - Duration::from_secs(1)
            },
            now
        ));
    }

    #[tokio::test]
    async fn error_mapping_surfaces_code_and_msg() {
        let (client, _mock) = mock_client(vec![
            r#"{"errorCode":-44116,"msg":"Invalid token","result":null}"#.to_string(),
        ]);
        let err = client.vlans(None).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("-44116"), "error was: {msg}");
        assert!(msg.contains("Invalid token"), "error was: {msg}");
    }

    #[tokio::test]
    async fn pagination_stops_on_total_rows() {
        let (client, mock) = mock_client(vec![
            list_page(250, 100),
            list_page(250, 100),
            list_page(250, 50),
        ]);
        let vlans = client.vlans(None).await.unwrap();
        assert_eq!(vlans.len(), 250);
        let urls = mock.urls.lock().await;
        assert_eq!(
            urls.len(),
            3,
            "expected exactly 3 page requests, got {urls:?}"
        );
        assert!(urls[0].contains("page=1&pageSize=100"), "url0: {}", urls[0]);
        assert!(urls[1].contains("page=2"), "url1: {}", urls[1]);
        assert!(urls[2].contains("page=3"), "url2: {}", urls[2]);
    }

    #[tokio::test]
    async fn pagination_terminates_on_empty_page_despite_total_rows() {
        let (client, mock) = mock_client(vec![list_page(999, 2), list_page(999, 0)]);
        let vlans = client.vlans(None).await.unwrap();
        assert_eq!(vlans.len(), 2);
        assert_eq!(mock.urls.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn concurrent_first_calls_authenticate_once() {
        let (client, mock) = fresh_client(vec![
            AUTH_RESPONSE.to_string(),
            EMPTY_PAGE.to_string(),
            EMPTY_PAGE.to_string(),
        ]);
        let (a, b) = tokio::join!(client.vlans(None), client.vlans(None));
        assert!(a.is_ok(), "call a: {:?}", a.err());
        assert!(b.is_ok(), "call b: {:?}", b.err());
        assert_eq!(
            *mock.auth_calls.lock().await,
            1,
            "auth must happen exactly once"
        );
    }

    #[test]
    fn retry_predicate_distinguishes_transport_from_api_errors() {
        // transport-level: the marker in the chain
        let transport_err = anyhow!(TransportFailure).context("request to https://om.test failed");
        assert!(is_transport_error(&transport_err));
        // API-level: envelope error after a full body was received
        let api_err = anyhow!("Omada error -44116: Invalid token");
        assert!(!is_transport_error(&api_err));
    }
}
