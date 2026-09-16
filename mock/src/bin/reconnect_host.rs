//! Test-only harness binary: repeats the connect/run/teardown sequence in
//! *one process*, `--cycles` times, using `anazoa_tun::host::TunnelSession`.
//!
//! This exists to reproduce what Android's `nativeStart`/`nativeStop` cycle
//! does to a loaded `.so` — reuse process-global statics (`TUNNEL_STATE`,
//! `SHUTDOWN_TX`, ...) across repeated start/stop cycles — which a normal
//! `anazoa-tun` invocation never exercises (a fresh process per run has
//! nothing to reuse). See `mock test --reconnect`, which spawns this in
//! place of `anazoa-tun` for the caller side and drives it exactly like the
//! desktop CLI: over `anazoa-ctl` against the config's `ctl-socket`,
//! including a `ctl shutdown` between cycles to signal "disconnect now,
//! reconnect for the next cycle."
//!
//! Deliberately lives here rather than as a flag on `anazoa-tun` itself:
//! this reconnect loop has no reason to exist outside a test harness, and a
//! CLI flag on the production binary would suggest otherwise.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use anazoa_tun::engine;
use anazoa_tun::host::TunnelSession;
use anazoa_tun::protozoa::tunnel::{keep_opus_hooks_linked, keep_vp9_hooks_linked};

fn usage() -> ! {
    eprintln!("usage: reconnect-host -c config.toml [--cycles N]");
    std::process::exit(1);
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut config_path = "anazoa.toml".to_string();
    let mut cycles: u32 = 2;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" => {
                config_path = args
                    .next()
                    .ok_or_else(|| anyhow!("-c requires an argument"))?;
            }
            "--cycles" => {
                cycles = args
                    .next()
                    .ok_or_else(|| anyhow!("--cycles requires an argument"))?
                    .parse()
                    .map_err(|_| anyhow!("--cycles expects an integer"))?;
            }
            _ => usage(),
        }
    }

    let cfg: anazoa_tun::Config = anazoa_config::load_config(&config_path)?;
    anazoa_config::init_logging(&cfg.auth.debug.level);
    if cfg.auth.debug.log_signaling_ws {
        anazoa_auth::signaling::set_log_signaling_ws(true);
    }
    keep_vp9_hooks_linked();
    keep_opus_hooks_linked();
    engine::init_noise_from_config(&cfg)?;

    let tun_name = cfg.tun_name.clone();
    if tun_name.is_empty() {
        bail!("tun-name must not be empty");
    }
    let log_dir = cfg.auth.debug.log_dir.clone();
    let log_prefix = cfg
        .auth
        .debug
        .log_prefix
        .clone()
        .unwrap_or_else(|| tun_name.clone());
    let media_path = cfg.media.clone();
    let ctl_socket = cfg.ctl_socket.clone();

    for cycle in 0..cycles {
        if cycle > 0 {
            tracing::info!("reconnect-host: starting cycle {cycle}");
        }

        let build_tun_name = tun_name.clone();
        let session = TunnelSession::start(
            cfg.clone(),
            move || open_tun_with_retry(&build_tun_name),
            log_dir.clone(),
            log_prefix.clone(),
            media_path.clone(),
            // auto_call=false: unlike android.rs, this harness is driven by
            // explicit `ctl call` RPCs from the mock test harness (matching
            // the desktop CLI's control style), not an immediate auto-dial —
            // auto_call=true here would race the harness's own `ctl call`
            // with a second, unrequested outgoing call attempt.
            false,
            Some(&ctl_socket),
        )?;

        // Block until told to disconnect (a "ctl shutdown" RPC — the same
        // one Android's nativeStop drives via trigger_shutdown()) or the
        // session ends on its own (error). is_finished() is the only signal
        // TunnelSession exposes for "this session's work has concluded";
        // a short poll is fine for a test harness.
        while !session.is_finished() {
            std::thread::sleep(Duration::from_millis(50));
        }
        session.stop();
    }

    Ok(())
}

fn open_tun_with_retry(tun_name: &str) -> Result<tun_rs::AsyncDevice> {
    // The previous cycle's TunBridge::drop only *requests* its reader/writer
    // tasks abort; the kernel doesn't necessarily release the interface the
    // instant the next cycle tries to reopen it. A fresh single-shot process
    // never sees this (it only opens the device once); reopening it in the
    // same process, which is this harness's whole point, does. A few
    // retries clear it in practice.
    let mut attempts = 0;
    loop {
        match tun_rs::DeviceBuilder::new()
            .name(tun_name)
            .layer(tun_rs::Layer::L3)
            .build_async()
        {
            Ok(tun) => return Ok(tun),
            Err(err) if attempts < 20 => {
                attempts += 1;
                tracing::warn!("create TUN device (attempt {attempts}): {err:#}");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(err).context("create TUN device"),
        }
    }
}
