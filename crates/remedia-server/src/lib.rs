//! Authenticated, standalone WebSocket RPC server for the Remedia Rust engine.

mod config;
mod protocol;
mod rpc;
mod server;
mod state;

pub use config::{ConfigError, ServerConfig};
pub use protocol::PROTOCOL_VERSION;
pub use server::{ServerError, ServerHandle};
