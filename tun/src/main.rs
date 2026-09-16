use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc;
use tracing::warn;

use anazoa_config::load_config;
use anazoa_tun::daemon;
use anazoa_tun::engine;
use anazoa_tun::protozoa::media::RaylibMedia;
use anazoa_tun::protozoa::tunnel::{keep_opus_hooks_linked, keep_vp9_hooks_linked};

fn usage() -> ! {
    eprintln!("usage: anazoa-tun [-c config.toml]");
    std::process::exit(1);
}

fn version() -> ! {
    println!("anazoa-tun {}", env!("CARGO_PKG_VERSION"));
    std::process::exit(0);
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut config_path = "anazoa.toml".to_string();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" => version(),
            "-c" => {
                config_path = args
                    .next()
                    .ok_or_else(|| anyhow!("-c requires an argument"))?;
            }
            _ => usage(),
        }
    }

    let cfg: anazoa_tun::Config = load_config(&config_path)?;
    anazoa_config::init_logging(&cfg.auth.debug.level);
    anazoa_tun::init_shutdown_watcher();
    if cfg.auth.debug.log_signaling_ws {
        anazoa_auth::signaling::set_log_signaling_ws(true);
    }
    keep_vp9_hooks_linked();
    keep_opus_hooks_linked();
    engine::init_noise_from_config(&cfg)?;

    let log_dir = cfg.auth.debug.log_dir.as_deref().map(std::path::Path::new);
    let tun_name = cfg.tun_name.as_str();
    if tun_name.is_empty() {
        bail!("tun-name must not be empty");
    }
    let tun = tun_rs::DeviceBuilder::new()
        .name(tun_name)
        .layer(tun_rs::Layer::L3)
        .build_async()
        .context("create TUN device")?;
    let (video_width, video_height) = engine::video_resolution(&cfg)?;

    let log_prefix = cfg.auth.debug.log_prefix.as_deref().unwrap_or(tun_name);
    let _tun_bridge = anazoa_tun::protozoa::tunnel::start_tun_bridge(
        tun,
        log_dir,
        log_prefix,
        video_width,
        video_height,
    )
    .await?;

    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);

    // Bind the daemon socket while still privileged so the path (e.g. /run/)
    // is writable even when privdrop is configured.
    let jsonrpc_listener = match daemon::bind_socket(&cfg.ctl_socket) {
        Ok(l) => Some(l),
        Err(e) => {
            warn!("JSON-RPC server: {e:#}");
            None
        }
    };

    anazoa_tun::privdrop::maybe_drop_privileges(cfg.privdrop.as_deref())?;

    let mut media = cfg
        .media
        .as_deref()
        .map(|p| RaylibMedia::spawn(p, video_width, video_height))
        .transpose()?;

    if let Some(listener) = jsonrpc_listener {
        tokio::spawn({
            let cmd_tx = cmd_tx.clone();
            async move {
                if let Err(e) = daemon::run_jsonrpc_server(listener, cmd_tx).await {
                    warn!("JSON-RPC server: {e:#}");
                }
            }
        });
    }

    // The CLI answers status over the JSON-RPC socket via cmd_rx; nothing
    // here reads the watch-based snapshot, so its receiver is just dropped.
    let (status_tx, _status_rx) = tokio::sync::watch::channel(engine::EngineState::Connecting);
    engine::run_daemon(
        &cfg,
        &mut media,
        &mut cmd_rx,
        video_width,
        video_height,
        false,
        &status_tx,
    )
    .await?;

    anazoa_tun::protozoa::tunnel::finalize_webm();
    Ok(())
}
