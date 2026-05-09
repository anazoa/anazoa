use std::io::{self, Write};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::oneme::SessionClient;
use crate::{FingerprintConfig, ServiceEndpoints};

const DEFAULT_ONEME_KEEPALIVE_SECS: u64 = 25;

pub async fn run_login(phone: Option<String>, fingerprint: &FingerprintConfig) -> Result<()> {
    tracing::info!("Starting login with OneMe protocol");
    run_login_with_endpoints(
        phone,
        &ServiceEndpoints::default(),
        DEFAULT_ONEME_KEEPALIVE_SECS,
        fingerprint,
    )
    .await
}

pub async fn run_login_with_endpoints(
    phone: Option<String>,
    endpoints: &ServiceEndpoints,
    oneme_keepalive_secs: u64,
    fingerprint: &FingerprintConfig,
) -> Result<()> {
    let mut client = SessionClient::connect(
        endpoints,
        Duration::from_secs(oneme_keepalive_secs),
        fingerprint,
    )
    .await?;

    let phone = phone.ok_or_else(|| anyhow::anyhow!("login requires --phone"))?;
    let login_token = login_with_phone(&mut client, &phone).await?;

    client.do_chat_sync(&login_token).await?;
    let user_id = client
        .user_id()
        .ok_or_else(|| anyhow::anyhow!("missing OneMe user ID after chat sync"))?;
    let call_token = client.do_call_token_request().await?;
    let signaling_user_id = crate::calls::login_uid(&call_token).await?;

    println!("Authentication successful!");
    println!();
    println!("Add the following authentication settings to a local anazoa.toml:");
    println!("signaling-user-id = \"{}\"", signaling_user_id);
    println!("token = \"{}\"", login_token);
    println!();
    println!("Add your peer ID to a remote anazoa.toml:");
    println!("remote-peer-id = {}", user_id);

    Ok(())
}

async fn login_with_phone(client: &mut SessionClient, phone: &str) -> Result<String> {
    let verification_token = client.do_verification_request(phone).await?;

    print!("SMS code sent to {}. Enter code: ", phone);
    io::stdout().flush()?;

    let code = tokio::task::spawn_blocking(|| -> io::Result<String> {
        let mut line = String::new();
        let n = io::stdin().read_line(&mut line)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no input"));
        }
        Ok(line.trim().to_string())
    })
    .await
    .context("stdin reader thread panicked")?
    .context("read SMS code")?;

    client.do_code_enter(&verification_token, &code).await
}
