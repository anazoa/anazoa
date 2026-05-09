pub mod calls;
pub mod login;
pub mod oneme;
pub mod signaling;

pub use anazoa_config::{FingerprintConfig, ServiceEndpoints};
pub use login::{run_login, run_login_with_endpoints};

use serde::Deserialize;
use std::sync::OnceLock;

/// Unified ICE/TURN server credentials used for both incoming and outgoing calls.
#[derive(Clone, Debug, Deserialize)]
pub struct TurnServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

pub fn ensure_rustls_provider() {
    static RUSTLS_PROVIDER: OnceLock<()> = OnceLock::new();
    let _ = RUSTLS_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
