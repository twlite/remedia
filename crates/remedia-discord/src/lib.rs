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

pub use connection::{
    ConnectionState, DiscordOpusSender, DiscordVoiceConnection, VoiceConnectionInfo,
};
pub use error::VoiceError;
