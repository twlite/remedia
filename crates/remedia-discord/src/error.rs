use std::io;

use thiserror::Error;

/// Errors returned while establishing or using a Discord voice connection.
#[derive(Debug, Error)]
pub enum VoiceError {
    #[error("voice endpoint must be a valid secure WebSocket endpoint")]
    InvalidEndpoint,
    #[error("voice connection timed out while {0}")]
    Timeout(&'static str),
    #[error("voice gateway protocol error: {0}")]
    Protocol(String),
    #[error("voice gateway WebSocket failed: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("voice UDP operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("voice gateway JSON was invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("voice endpoint URL was invalid: {0}")]
    Url(#[from] url::ParseError),
    #[error("voice gateway did not offer a supported RTP-size encryption mode")]
    EncryptionUnavailable,
    #[error("voice gateway selected a different encryption mode than requested")]
    EncryptionModeMismatch,
    #[error("voice gateway session key must be exactly 32 bytes")]
    InvalidSessionKey,
    #[error("transport encryption failed: {0}")]
    Encryption(String),
    #[error("DAVE is required, but the voice gateway negotiated protocol version 0")]
    DaveRequired,
    #[error("DAVE protocol version {0} is not supported by the installed davey implementation")]
    UnsupportedDaveVersion(u16),
    #[error("DAVE session is not ready; refusing to send plaintext Opus")]
    DaveNotReady,
    #[error("DAVE protocol error: {0}")]
    Dave(String),
    #[error("RTP packet duration {0} samples per channel is not a legal Opus frame duration")]
    InvalidOpusDuration(u32),
    #[error("RTP packet would exceed the {maximum}-byte UDP limit (got {actual} bytes)")]
    PacketTooLarge { actual: usize, maximum: usize },
    #[error("the voice connection has been disconnected")]
    Disconnected,
    #[error("the voice connection is reconnecting")]
    Reconnecting,
    #[error("voice gateway resume failed: {0}")]
    ResumeFailed(String),
    #[error("voice output was cancelled")]
    Cancelled,
    #[error("voice playback output was superseded by a newer output instance")]
    Superseded,
    #[error("voice playback output has already finished")]
    OutputFinished,
    #[error("voice connection worker failed: {0}")]
    Worker(String),
}
