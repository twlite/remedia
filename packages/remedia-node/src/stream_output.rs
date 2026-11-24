use napi::bindgen_prelude::Buffer;
use napi_derive::napi;
use remedia_core::{
    MediaOutput, OpusEncoderConfig, StreamFormat, StreamOutput, StreamPacing, StreamReader,
};
use std::sync::Arc;

/// One owned audio chunk transferred to JavaScript without copying its payload.
#[napi(object)]
pub struct NativeAudioChunk {
    /// PCM bytes or exactly one Opus packet.
    pub data: Buffer,
    /// Start on the normalized 48 kHz output timeline.
    pub start_sample: f64,
    /// Encoded duration in samples per channel.
    pub samples_per_channel: u32,
    /// Media duration excluding padding.
    pub media_samples_per_channel: u32,
}

/// One-shot, bounded native output, consumed only by the TypeScript Readable adapter.
#[napi]
pub struct NativeAudioOutput {
    pub(crate) output: Arc<StreamOutput>,
    reader: napi::tokio::sync::Mutex<StreamReader>,
}

#[napi]
impl NativeAudioOutput {
    /// Create a stream output before playback starts; construction does not start production.
    /// @param format - s16le, f32le or opus output representation.
    /// @param pacing - unpaced (demand-driven) or realtime (sample-clock pacing).
    /// @param bitrate - Optional Opus target bits/s, 500–512000; default 128000.
    /// @param complexity - Optional Opus complexity, 0–10; default 9.
    /// @param vbr - Optional variable bitrate flag; default true.
    /// @returns One-shot output handle; rejects invalid output or encoder settings.
    #[napi(constructor)]
    pub fn new(
        format: String,
        pacing: String,
        bitrate: Option<i32>,
        complexity: Option<i32>,
        vbr: Option<bool>,
    ) -> napi::Result<Self> {
        let format = match format.as_str() {
            "s16le" => StreamFormat::S16le,
            "f32le" => StreamFormat::F32le,
            "opus" => StreamFormat::Opus,
            _ => return Err(napi::Error::from_reason("unsupported audio output format")),
        };
        let pacing = match pacing.as_str() {
            "unpaced" => StreamPacing::Unpaced,
            "realtime" => StreamPacing::Realtime,
            _ => return Err(napi::Error::from_reason("unsupported audio output pacing")),
        };
        let defaults = OpusEncoderConfig::default();
        let (output, reader) = StreamOutput::new(
            format,
            pacing,
            OpusEncoderConfig {
                bitrate: bitrate.unwrap_or(defaults.bitrate),
                complexity: complexity.unwrap_or(defaults.complexity),
                vbr: vbr.unwrap_or(defaults.vbr),
            },
        )
        .map_err(|error| napi::Error::from_reason(error.to_string()))?;
        Ok(Self {
            output: Arc::new(output),
            reader: napi::tokio::sync::Mutex::new(reader),
        })
    }

    /// Wait for downstream demand without occupying a blocking thread.
    /// @returns An owned chunk, null at EOF, or rejection on playback failure.
    #[napi]
    pub async fn read(&self) -> napi::Result<Option<NativeAudioChunk>> {
        self.reader
            .lock()
            .await
            .next()
            .await
            .map(|chunk| {
                chunk.map(|chunk| NativeAudioChunk {
                    data: chunk.data.into(),
                    start_sample: chunk.start_sample as f64,
                    samples_per_channel: chunk.samples_per_channel,
                    media_samples_per_channel: chunk.media_samples_per_channel,
                })
            })
            .map_err(|error| napi::Error::from_reason(error.to_string()))
    }

    /// Wake both sides and release queued native payloads, including a pending read.
    /// @returns No value. Later playback outputs are unaffected.
    #[napi]
    pub fn cancel(&self) {
        self.output.cancel();
    }
}
