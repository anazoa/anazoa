use anyhow::{Context, Result, anyhow, bail};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::{Method, Request, Uri};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use serde::Serialize;
use std::sync::OnceLock;
use unicode_truncate::UnicodeTruncateStr;
use uuid::Uuid;

use crate::{TurnServer, ensure_rustls_provider};

const APPLICATION_KEY: &str = "CGPGAGLGDIHBABABA";

#[derive(Serialize)]
struct SessionData<'a> {
    auth_token: &'a str,
    client_version: &'static str,
    device_id: String,
    version: i32,
}

#[derive(Deserialize)]
struct LoginData {
    uid: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StunServer {
    pub urls: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StartedConversationInfo {
    #[serde(rename = "turn")]
    pub turn_server: TurnServer,
    #[serde(rename = "stun")]
    pub stun_server: StunServer,
    pub endpoint: String,
    #[serde(rename = "wtEndpoint")]
    pub wt_endpoint: Option<String>,
}

type RequestBody = Full<Bytes>;
type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, RequestBody>;

pub async fn login_uid(call_token: &str) -> Result<String> {
    let device_id = Uuid::new_v4().simple().to_string();
    let device_id = &device_id[..16];
    let session_data = SessionData {
        auth_token: call_token,
        client_version: "1",
        device_id: device_id.to_string(),
        version: 3,
    };

    let session_data_json = serde_json::to_string(&session_data)?;
    let login_data: LoginData = post_form_json(
        "https://calls.okcdn.ru/fb.do",
        &[
            ("method", "auth.anonymLogin"),
            ("format", "JSON"),
            ("application_key", APPLICATION_KEY),
            ("client", "test"),
            ("deviceId", device_id),
            ("gen_token", "true"),
            ("session_data", &session_data_json),
            ("verification_supported", "true"),
            ("verification_supported_v", "1"),
        ],
    )
    .await?;

    Ok(login_data.uid)
}

fn client() -> Result<&'static HttpsClient> {
    static CLIENT: OnceLock<Result<HttpsClient, String>> = OnceLock::new();
    let client_result = CLIENT.get_or_init(|| {
        ensure_rustls_provider();
        let https = match HttpsConnectorBuilder::new().with_native_roots() {
            Ok(https) => https,
            Err(err) => return Err(format!("load native root certificates: {err}")),
        }
        .https_or_http()
        .enable_http1()
        .build();
        Ok(Client::builder(TokioExecutor::new()).build(https))
    });
    client_result.as_ref().map_err(|err| anyhow!(err.clone()))
}

async fn post_form_json<T: serde::de::DeserializeOwned>(
    url: &str,
    form: &[(&str, &str)],
) -> Result<T> {
    let body = post_form_bytes(url, form).await?;
    serde_json::from_slice(&body).context("decode JSON response")
}

async fn post_form_bytes(url: &str, form: &[(&str, &str)]) -> Result<Bytes> {
    let encoded = serde_urlencoded::to_string(form).context("encode form body")?;
    let request = Request::builder()
        .method(Method::POST)
        .uri(parse_uri(url)?)
        .header(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        )
        .body(Full::new(Bytes::from(encoded)))
        .context("build POST request")?;
    let response = client()?
        .request(request)
        .await
        .context("send POST request")?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .context("read POST response body")?
        .to_bytes();
    if !status.is_success() {
        let body_text = String::from_utf8_lossy(&body);
        let (short, len) = body_text.unicode_truncate(200);
        bail!(
            "POST {} returned {} body={}{}",
            url,
            status,
            short,
            if len < body_text.len() { "..." } else { "" }
        );
    }
    Ok(body)
}

fn parse_uri(url: &str) -> Result<Uri> {
    url.parse::<Uri>()
        .map_err(|e| anyhow!("parse URI {}: {}", url, e))
}
