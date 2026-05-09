use anyhow::{Result, anyhow};
use serde::Deserialize;

use anazoa_auth::run_login;
use anazoa_config::{AuthConfig, init_logging, load_config};

#[derive(Debug, Deserialize, Clone)]
pub struct LocalAuthConfig {
    #[serde(flatten)]
    pub auth: AuthConfig,
}

fn usage() -> ! {
    eprintln!("usage: anazoa-auth [-c config.toml] login --phone <phone>");
    std::process::exit(1);
}

fn version() -> ! {
    println!("anazoa-auth {}", env!("CARGO_PKG_VERSION"));
    std::process::exit(0);
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut config_path = "anazoa.toml".to_string();
    let mut phone = None;
    let mut login_cmd = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" => version(),
            "-c" => {
                config_path = args
                    .next()
                    .ok_or_else(|| anyhow!("-c requires an argument"))?;
            }
            "login" => {
                login_cmd = true;
            }
            "--phone" => {
                phone = Some(
                    args.next()
                        .ok_or_else(|| anyhow!("--phone requires an argument"))?,
                );
            }
            _ => usage(),
        }
    }

    if !login_cmd {
        usage();
    }

    let cfg: LocalAuthConfig = load_config(&config_path)?;
    init_logging(&cfg.auth.debug.level);

    run_login(phone, &cfg.auth.fingerprint).await?;

    Ok(())
}
