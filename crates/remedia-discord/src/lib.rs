//! Minimal outbound Discord voice transport.
//!
//! The caller obtains voice session information from its Discord Gateway
//! client, then gives this crate transport-independent Opus packets. This
//! crate owns the voice WebSocket, UDP socket, DAVE session, RTP packetization,
//! encryption, and real-time scheduling. It does not decode or encode audio.

mod connection;
mod crypto;
mod dave;
mod error;
mod protocol;
mod rtp;
mod sender_router;

use remedia_observability::ObservabilityHandle;

pub(crate) fn with_dispatch<T>(handle: &ObservabilityHandle, callback: impl FnOnce() -> T) -> T {
    match handle.dispatch() {
        Some(dispatch) => tracing::dispatcher::with_default(dispatch, callback),
        None => callback(),
    }
}

pub use connection::{
    ConnectionState, DiscordOpusSender, DiscordVoiceConnection, VoiceConnectionInfo,
};
pub use error::VoiceError;
pub use sender_router::{DiscordOpusSenderRouter, RoutedOpusSender};
