use std::io::{self, Write};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::oneme::{CodeOutcome, SessionClient};
use crate::{FingerprintConfig, ServiceEndpoints};

pub async fn run_login_with_endpoints(
    phone: &str,
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

    let login_token = login_with_phone(&mut client, phone).await?;

    client.do_chat_sync(&login_token).await?;
    let user_id = client.user_id().ok_or_else(|| {
        anyhow::anyhow!("could not determine OneMe user ID: not in the login payload")
    })?;
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
    let code = prompt(&format!("SMS code sent to {phone}. Enter code: ")).await?;

    match client.do_code_enter(&verification_token, &code).await? {
        CodeOutcome::LoggedIn(token) => Ok(token),
        CodeOutcome::PasswordRequired { track_id, hint } => {
            match hint {
                Some(hint) => println!("This account has a login password. Hint: {hint}"),
                None => println!("This account has a login password."),
            }
            let password = prompt("Enter login password: ").await?;
            client.do_password_check(&track_id, &password).await
        }
    }
}

/// Print `msg` (no newline) and read one trimmed line from stdin.
async fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    io::stdout().flush()?;

    tokio::task::spawn_blocking(|| -> io::Result<String> {
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no input"));
        }
        Ok(line.trim().to_string())
    })
    .await
    .context("stdin reader thread panicked")?
    .context("read line from stdin")
}
