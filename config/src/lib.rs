use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::collections::VecDeque;
use std::fs;
use std::sync::{Mutex, OnceLock};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServiceEndpoints {
    #[serde(rename = "oneme-web-url")]
    pub oneme_web_url: String,
    #[serde(rename = "oneme-api-url")]
    pub oneme_api_url: String,
    /// Skip TLS certificate verification. Only set this for local testing.
    #[serde(rename = "skip-tls-verify", default)]
    pub skip_tls_verify: bool,
}

impl Default for ServiceEndpoints {
    fn default() -> Self {
        Self {
            oneme_web_url: "https://web.max.ru".to_string(),
            oneme_api_url: "https://api.oneme.ru".to_string(),
            skip_tls_verify: false,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct FingerprintConfig {
    #[serde(rename = "os-version")]
    pub os_version: String,
    /// Android API level integer (e.g. 34 for Android 14). Used in signaling URL `osVersion` param.
    #[serde(rename = "os-api-level")]
    pub os_api_level: u32,
    pub timezone: String,
    pub screen: String,
    pub locale: String,
    #[serde(rename = "device-name")]
    pub device_name: String,
    #[serde(rename = "device-locale")]
    pub device_locale: String,
    #[serde(rename = "device-id")]
    pub device_id: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DebugConfig {
    #[serde(rename = "log-signaling-ws", default)]
    pub log_signaling_ws: bool,
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(rename = "log-dir")]
    pub log_dir: Option<String>,
    #[serde(rename = "log-prefix")]
    pub log_prefix: Option<String>,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            log_signaling_ws: false,
            level: default_log_level(),
            log_dir: None,
            log_prefix: None,
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

pub fn init_logging(default_level: &str) {
    let level_str = std::env::var("RUST_LOG").unwrap_or_else(|_| default_level.to_string());
    let level = parse_log_level(&level_str);
    let stdout_targets = tracing_subscriber::filter::Targets::new().with_default(level);
    let ring_targets = tracing_subscriber::filter::Targets::new().with_default(level);
    if let Err(err) = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_level(true)
                .with_filter(stdout_targets),
        )
        // Mirrors formatted lines into an in-memory ring buffer alongside
        // stdout, purely additive — desktop behavior is unchanged. Read back
        // via `recent_log_lines()`; the Android app polls this for its
        // in-app log view, since stdout isn't visible there.
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_level(true)
                .with_ansi(false)
                .with_writer(|| RingWriter)
                .with_filter(ring_targets),
        )
        .try_init()
    {
        eprintln!("logging already initialized; keeping existing subscriber: {err}");
    }
}

const LOG_RING_CAPACITY: usize = 300;

static LOG_RING: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

fn log_ring() -> &'static Mutex<VecDeque<String>> {
    LOG_RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(LOG_RING_CAPACITY)))
}

struct RingWriter;

impl std::io::Write for RingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // tracing-subscriber's fmt layer builds one complete formatted line
        // (with trailing '\n') per event and issues a single write_all with
        // it, so treating each call as >=1 complete lines is safe in
        // practice; split_inclusive still copes if that ever changes within
        // one call.
        let text = String::from_utf8_lossy(buf);
        let mut ring = log_ring().lock().unwrap();
        for line in text.split_inclusive('\n') {
            let line = line.trim_end_matches('\n');
            if line.is_empty() {
                continue;
            }
            if ring.len() >= LOG_RING_CAPACITY {
                ring.pop_front();
            }
            ring.push_back(line.to_string());
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Recent formatted log lines captured by [`init_logging`], oldest first.
pub fn recent_log_lines() -> Vec<String> {
    log_ring().lock().unwrap().iter().cloned().collect()
}

fn parse_log_level(level: &str) -> LevelFilter {
    match level.to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "info" => LevelFilter::INFO,
        "warn" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        "off" => LevelFilter::OFF,
        _ => {
            eprintln!("warning: unrecognised log level {level:?}, defaulting to INFO");
            LevelFilter::INFO
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    #[serde(default)]
    pub endpoints: ServiceEndpoints,
    #[serde(default)]
    pub debug: DebugConfig,
    pub fingerprint: FingerprintConfig,
}

pub fn load_config<T: serde::de::DeserializeOwned>(path: &str) -> Result<T> {
    let contents = fs::read_to_string(path).map_err(|e| anyhow!("read config {}: {}", path, e))?;
    toml::from_str(&contents).map_err(|e| anyhow!("parse config {}: {}", path, e))
}
