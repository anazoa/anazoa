use anyhow::{Result, anyhow};
use serde::Deserialize;

use anazoa_auth::run_login_with_endpoints;
use anazoa_config::{AuthConfig, init_logging, load_config};

const DEFAULT_ONEME_KEEPALIVE_SECS: u64 = 25;

#[derive(Debug, Deserialize, Clone)]
pub struct LocalAuthConfig {
    #[serde(flatten)]
    pub auth: AuthConfig,
}

fn usage() -> ! {
    eprintln!("usage: anazoa-auth [-c config.toml] login --phone <phone>");
    eprintln!("       anazoa-auth genkey");
    std::process::exit(1);
}

fn genkey() -> ! {
    use base64::Engine as _;
    let pattern: snow::params::NoiseParams = "Noise_KK_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
    let keypair = snow::Builder::new(pattern)
        .generate_keypair()
        .expect("generate keypair");
    let enc = base64::engine::general_purpose::STANDARD;
    println!("noise-privkey = \"{}\"", enc.encode(&keypair.private));
    println!(
        "noise-peer-pubkey = \"{}\"  # share this with the other peer",
        enc.encode(&keypair.public)
    );
    std::process::exit(0);
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
            "genkey" => genkey(),
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

    run_login_with_endpoints(phone, &cfg.auth.endpoints, DEFAULT_ONEME_KEEPALIVE_SECS, &cfg.auth.fingerprint).await?;

    Ok(())
}
