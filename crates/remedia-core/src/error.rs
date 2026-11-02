use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecoderError {
    #[error("FFmpeg initialization failed: {0}")]
    Initialization(String),
    #[error("invalid media URL: {0}")]
    InvalidUrl(String),
    #[error("only HTTP and HTTPS media URLs are supported")]
    UnsupportedUrlScheme,
    #[error("media file paths must be valid Unicode")]
    NonUnicodePath,
    #[error("media source contains a NUL byte")]
    InvalidSource,
    #[error("media source was not found")]
    SourceNotFound,
    #[error("permission was denied while opening the media source")]
    SourceInaccessible,
    #[error("input is not a supported media format: {0}")]
    UnsupportedMedia(String),
    #[error("media data is invalid or truncated: {0}")]
    InvalidMedia(String),
    #[error("media input timed out")]
    InputTimeout,
    #[error("media input was cancelled")]
    Cancelled,
    #[error("HTTP request failed with status {0}")]
    HttpStatus(u16),
    #[error("failed to open media input: {0}")]
    OpenInput(String),
    #[error("media input contains no usable audio stream")]
    NoAudioStream,
    #[error("no decoder is available for audio codec `{0}`")]
    DecoderUnavailable(String),
    #[error("audio decoder could not be initialized: {0}")]
    DecoderInitialization(String),
    #[error("network request failed: {0}")]
    NetworkFailure(String),
    #[error("audio decoding failed: {0}")]
    Decode(String),
    #[error("audio conversion failed: {0}")]
    Resample(String),
    #[error("decoder configuration is invalid: {0}")]
    InvalidOptions(&'static str),
    #[error("FFmpeg returned invalid audio format information")]
    InvalidAudioFormat,
    #[error("the audio format changed while decoding")]
    FormatChanged,
    #[error("the decoded audio frame is too large")]
    FrameTooLarge,
    #[error("seeking is not supported by this media source: {0}")]
    UnsupportedSeeking(String),
    #[error("seek position {target:?} is beyond media duration {duration:?}")]
    SeekOutOfRange {
        target: Duration,
        duration: Duration,
    },
}

pub(crate) fn ffmpeg_error(
    error: ffmpeg_next::Error,
    operation: &'static str,
    cancelled: bool,
    network_input: bool,
) -> DecoderError {
    if cancelled {
        return DecoderError::Cancelled;
    }

    match error {
        ffmpeg_next::Error::HttpBadRequest => DecoderError::HttpStatus(400),
        ffmpeg_next::Error::HttpUnauthorized => DecoderError::HttpStatus(401),
        ffmpeg_next::Error::HttpForbidden => DecoderError::HttpStatus(403),
        ffmpeg_next::Error::HttpNotFound => DecoderError::HttpStatus(404),
        ffmpeg_next::Error::HttpServerError => {
            DecoderError::NetworkFailure(format!("HTTP server error while {operation}"))
        }
        ffmpeg_next::Error::HttpOther4xx => {
            DecoderError::NetworkFailure(format!("HTTP client error while {operation}"))
        }
        ffmpeg_next::Error::Other {
            errno: ffmpeg_next::error::ETIMEDOUT,
        } => DecoderError::InputTimeout,
        ffmpeg_next::Error::Other {
            errno: ffmpeg_next::error::ENOENT,
        } => DecoderError::SourceNotFound,
        ffmpeg_next::Error::Other {
            errno: ffmpeg_next::error::EACCES | ffmpeg_next::error::EPERM,
        } => DecoderError::SourceInaccessible,
        ffmpeg_next::Error::DemuxerNotFound | ffmpeg_next::Error::ProtocolNotFound => {
            DecoderError::UnsupportedMedia(error.to_string())
        }
        ffmpeg_next::Error::InvalidData => {
            DecoderError::InvalidMedia(format!("{operation}: {error}"))
        }
        ffmpeg_next::Error::Exit => {
            let message = format!("{operation}: FFmpeg interrupted the input");
            if network_input {
                DecoderError::NetworkFailure(message)
            } else {
                DecoderError::OpenInput(message)
            }
        }
        error if network_input => DecoderError::NetworkFailure(format!("{operation}: {error}")),
        error => DecoderError::OpenInput(format!("{operation}: {error}")),
    }
}
