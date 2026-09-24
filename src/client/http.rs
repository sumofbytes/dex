//! Dex application defaults around the standalone client crate.

use std::ops::Deref;

pub use dex_client::http::{mcp_auth_lines, ChatOptions, ChatStream};

/// Application-configured client retaining Dex's shared runtime, HTTP pools,
/// credential lookup, and structured warning logger.
#[derive(Clone)]
pub struct DaemonClient(dex_client::DaemonClient);

impl DaemonClient {
    pub fn new(base_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        Self::with_token(base_url, crate::auth::client_daemon_token())
    }

    pub fn with_token(
        base_url: &str,
        token: Option<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let client = dex_client::DaemonClient::with_token_and_clients(
            base_url,
            token,
            crate::runtime::http::shared_async_client(),
            crate::runtime::http::shared_streaming_client(),
        )?
        .with_warning_handler(|message| crate::log!(Warn, "{message}"));
        Ok(Self(client))
    }
}

impl Deref for DaemonClient {
    type Target = dex_client::DaemonClient;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
