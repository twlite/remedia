//! Authenticated, standalone WebSocket RPC server for the Remedia Rust engine.

mod config;
mod plugin_host;
mod probe;
mod protocol;
mod rpc;
mod server;
mod state;

pub use config::{ConfigError, ServerConfig};
pub use plugin_host::{HostDiagnostic, diagnose_plugin_hosts};
pub use protocol::PROTOCOL_VERSION;
pub use server::{ServerError, ServerHandle};
