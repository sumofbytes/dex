mod agent;
pub mod app;
mod auth;
pub mod cli;
pub mod client;
pub mod daemon;
mod extensions;
mod llm;
mod mcp;
pub mod protocol;
mod render;
mod runtime;
mod session;
mod telemetry;
#[cfg(test)]
pub(crate) mod test_env;
mod tools;
#[cfg(feature = "tui")]
mod ui;
mod workspace;

pub use app::run;
