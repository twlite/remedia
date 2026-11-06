use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use opus2::{Application, Bitrate, Channels, Encoder};
use remedia_observability::ObservabilityHandle;

use crate::decoder::{OUTPUT_SAMPLE_RATE_HZ, PcmFrame};
use crate::output::{EncodedOpusOutput, MediaOutput, OutputError};
use crate::source::CancellationToken;
use crate::with_dispatch;

const OPUS_FRAME_SAMPLES: usize = 960;
const OPUS_FRAME_SCALARS: usize = OPUS_FRAME_SAMPLES * 2;
const MAX_OPUS_PACKET_BYTES: usize = 4_000;

/// Per-output settings for PCM-to-Opus encoding, independent of any transport.
///
/// These settings never affect source Opus packet passthrough. Encoding remains
/// 48 kHz stereo, with 20 ms frames and libopus's `Application::Audio` mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpusEncoderConfig {
    /// Target bitrate in bits per second, in libopus's documented 500..=512000 range.
    pub bitrate: i32,
    /// Computational complexity, from 0 (least CPU) to 10 (most CPU).
    pub complexity: i32,
    /// Allow variable bitrate to distribute bits according to audio complexity.
    pub vbr: bool,
}

impl Default for OpusEncoderConfig {
    fn default() -> Self {
        Self {
            bitrate: 128_000,
            complexity: 9,
            vbr: true,
        }
    }
}

impl OpusEncoderConfig {
    /// Validate settings without allocating a native encoder.
    pub fn validate(&self) -> Result<(), OutputError> {
        // libopus accepts some out-of-range rates by silently clamping them.
        // Use its documented meaningful range instead of accepting clamped values.
        if !(500..=512_000).contains(&self.bitrate) {
            return Err(OutputError::new(
                "audio.opus.bitrate must be between 500 and 512000 bits per second",
            ));
        }
        if !(0..=10).contains(&self.complexity) {
            return Err(OutputError::new(
                "audio.opus.complexity must be between 0 and 10",
            ));
        }
        Ok(())
    }
}

/// An owned Opus packet independent of any transport or encryption protocol.
///
/// `samples_per_channel` is the encoded Opus frame duration. The encoder emits
/// 960 samples per channel (20 ms) for every packet. `media_samples_per_channel`
/// counts source audio represented by the packet; it can be shorter only for
/// the final zero-padded frame. `start_sample` is the start on the normalized
/// 48 kHz media timeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedOpusPacket {
    pub bytes: Vec<u8>,
    pub samples_per_channel: u32,
    pub media_samples_per_channel: u32,
    pub start_sample: u64,
}

/// A synchronous, backpressured consumer of encoded Opus packets.
///
/// Implementations must bound their pending packet storage. The playback
/// worker calls these methods; they must not block an async runtime worker.
/// `cancel` may run concurrently and must unblock an in-progress method.
pub trait OpusPacketSink: Send + Sync + 'static {
    /// Accept an owned encoded packet.
    fn send_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError>;

    /// Return whether this sink can accept the encoded packet without
    /// transcoding. A transport can conservatively reject packets that exceed
    /// its packet-size or duration constraints.
    fn supports_passthrough_packet(&self, _packet: &EncodedOpusPacket) -> bool {
        true
    }

    /// Discard packets queued before a seek and move the media timeline.
    fn flush(
        &self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError>;

    /// Discard pre-seek packets and synchronize to an exact normalized PCM
    /// sample. Existing sinks remain compatible through the duration fallback.
    fn flush_samples(
        &self,
        sample_position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.flush(duration_from_samples(sample_position), cancellation)
    }

    /// Suspend transport after already accepted packets are drained.
    fn pause(&self, _cancellation: &CancellationToken) -> Result<(), OutputError> {
        Ok(())
    }

    /// Resume a previously paused transport.
    fn resume(&self, _cancellation: &CancellationToken) -> Result<(), OutputError> {
        Ok(())
    }

    /// Finish after all accepted packets are delivered.
    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError>;

    /// Cancel this output instance without necessarily closing its transport.
    fn cancel(&self);

    /// Report a consumed or played position only when the sink can measure it.
    fn played_position(&self) -> Option<Duration> {
        None
    }
}

/// Adapts normalized PCM frames to 20 ms stereo Opus packets.
///
/// It owns one lazily initialized native libopus encoder and one packet sink.
/// At most one partial frame (under 20 ms) is retained. A short final frame is zero-padded to 20 ms;
/// the packet records both the 20 ms encoded duration and its shorter media
/// duration. Seeking discards partial PCM and resets the encoder state.
pub struct OpusEncoderOutput {
    config: OpusEncoderConfig,
    state: Mutex<EncoderState>,
    sink: Box<dyn OpusPacketSink>,
    cancelled: AtomicBool,
    observability: ObservabilityHandle,
}

struct EncoderState {
    encoder: Option<Encoder>,
    partial: [f32; OPUS_FRAME_SCALARS],
    partial_scalars: usize,
    partial_start_sample: Option<u64>,
    next_input_sample: Option<u64>,
    finished: bool,
}

impl OpusEncoderOutput {
    /// Create an Opus output and attach an owned packet sink.
    ///
    /// The native libopus encoder is initialized lazily if playback requires
    /// PCM processing; a passthrough-only source does not construct it.
    pub fn new(sink: impl OpusPacketSink) -> Result<Self, OutputError> {
        Self::new_with_observability(sink, ObservabilityHandle::default())
    }

    /// Create the encoder with an application-owned local diagnostic context.
    pub fn new_with_observability(
        sink: impl OpusPacketSink,
        observability: ObservabilityHandle,
    ) -> Result<Self, OutputError> {
        Self::new_with_config_and_observability(sink, OpusEncoderConfig::default(), observability)
    }

    /// Create a lazily initialized output with validated, per-instance settings.
    /// Native initialization and control errors are returned on the first PCM write.
    pub fn new_with_config(
        sink: impl OpusPacketSink,
        config: OpusEncoderConfig,
    ) -> Result<Self, OutputError> {
        Self::new_with_config_and_observability(sink, config, ObservabilityHandle::default())
    }

    /// Create a configured output with an application-owned diagnostic context.
    pub fn new_with_config_and_observability(
        sink: impl OpusPacketSink,
        config: OpusEncoderConfig,
        observability: ObservabilityHandle,
    ) -> Result<Self, OutputError> {
        config.validate()?;
        Ok(Self {
            config,
            state: Mutex::new(EncoderState {
                // A passthrough-only source does not need a native encoder.
                // Construct libopus only when the first PCM frame arrives.
                encoder: None,
                partial: [0.0; OPUS_FRAME_SCALARS],
                partial_scalars: 0,
                partial_start_sample: None,
                next_input_sample: None,
                finished: false,
            }),
            sink: Box::new(sink),
            cancelled: AtomicBool::new(false),
            observability,
        })
    }

    fn initialize_encoder(&self, state: &mut EncoderState) -> Result<(), OutputError> {
        if state.encoder.is_some() {
            return Ok(());
        }
        let mut encoder = Encoder::new(OUTPUT_SAMPLE_RATE_HZ, Channels::Stereo, Application::Audio)
            .map_err(|error| {
                with_dispatch(&self.observability, || {
                    tracing::error!(
                        name: "core.opus.encoder_initialization_failed",
                        target: "remedia_core::opus",
                        error_kind = "native_encoder",
                        "libopus encoder initialization failed"
                    );
                });
                OutputError::with_source(error)
            })?;
        encoder
            .set_bitrate(Bitrate::Bits(self.config.bitrate))
            .map_err(OutputError::with_source)?;
        encoder
            .set_complexity(self.config.complexity)
            .map_err(OutputError::with_source)?;
        encoder
            .set_vbr(self.config.vbr)
            .map_err(OutputError::with_source)?;
        state.encoder = Some(encoder);
        with_dispatch(&self.observability, || {
            tracing::info!(
                name: "core.opus.encoder_initialized",
                target: "remedia_core::opus",
                sample_rate_hz = OUTPUT_SAMPLE_RATE_HZ,
                channels = 2_u16,
                frame_samples_per_channel = OPUS_FRAME_SAMPLES,
                "libopus encoder initialized"
            );
        });
        Ok(())
    }

    fn ensure_active(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        if self.cancelled.load(Ordering::Acquire) || cancellation.is_cancelled() {
            Err(OutputError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn encode_and_send(
        encoder: &mut Encoder,
        pcm: &[f32],
        encoded_samples_per_channel: usize,
        media_samples_per_channel: usize,
        start_sample: u64,
        sink: &dyn OpusPacketSink,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        let mut bytes = vec![0; MAX_OPUS_PACKET_BYTES];
        let len = encoder.encode_float(pcm, &mut bytes).map_err(|error| {
            tracing::error!(
                name: "core.opus.encode_failed",
                target: "remedia_core::opus",
                error_kind = "native_encode",
                "libopus could not encode a PCM frame"
            );
            OutputError::with_source(error)
        })?;
        bytes.truncate(len);
        let media_samples_per_channel = u32::try_from(media_samples_per_channel)
            .map_err(|_| OutputError::new("Opus frame sample count exceeds u32"))?;
        let packet_bytes = bytes.len();
        sink.send_packet(
            EncodedOpusPacket {
                bytes,
                samples_per_channel: u32::try_from(encoded_samples_per_channel)
                    .map_err(|_| OutputError::new("Opus frame sample count exceeds u32"))?,
                media_samples_per_channel,
                start_sample,
            },
            cancellation,
        )?;
        tracing::trace!(
            name: "core.opus.packet_encoded",
            target: "remedia_core::opus",
            encoded_samples_per_channel,
            media_samples_per_channel,
            start_sample,
            packet_bytes,
            "Opus packet encoded and accepted by sink"
        );
        Ok(())
    }

    fn write_pcm(
        state: &mut EncoderState,
        frame: PcmFrame,
        sink: &dyn OpusPacketSink,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        let scalar_count = frame
            .samples_per_channel
            .checked_mul(2)
            .ok_or_else(|| OutputError::new("PCM frame sample count overflowed"))?;
        if scalar_count != frame.samples.len() {
            return Err(OutputError::new(
                "PCM frame does not contain two interleaved samples per channel frame",
            ));
        }
        if state.finished {
            return Err(OutputError::new("PCM frame written after output finished"));
        }
        if let Some(expected) = state.next_input_sample
            && expected != frame.start_sample
        {
            tracing::error!(
                name: "core.opus.pcm_discontinuity",
                target: "remedia_core::opus",
                expected_sample = expected,
                received_sample = frame.start_sample,
                "Opus output rejected a discontinuous PCM timeline"
            );
            return Err(OutputError::new(format!(
                "PCM timeline is discontinuous: expected sample {expected}, received {}",
                frame.start_sample
            )));
        }

        let mut source_offset = 0;
        while source_offset < scalar_count {
            if state.partial_scalars == 0 {
                state.partial_start_sample =
                    Some(frame.start_sample + u64::try_from(source_offset / 2).unwrap_or(u64::MAX));
            }

            if state.partial_scalars == 0 && scalar_count - source_offset >= OPUS_FRAME_SCALARS {
                let start_sample = state
                    .partial_start_sample
                    .take()
                    .unwrap_or(frame.start_sample);
                Self::encode_and_send(
                    state
                        .encoder
                        .as_mut()
                        .ok_or_else(|| OutputError::new("Opus encoder is not initialized"))?,
                    &frame.samples[source_offset..source_offset + OPUS_FRAME_SCALARS],
                    OPUS_FRAME_SAMPLES,
                    OPUS_FRAME_SAMPLES,
                    start_sample,
                    sink,
                    cancellation,
                )?;
                source_offset += OPUS_FRAME_SCALARS;
                continue;
            }

            let count =
                (OPUS_FRAME_SCALARS - state.partial_scalars).min(scalar_count - source_offset);
            state.partial[state.partial_scalars..state.partial_scalars + count]
                .copy_from_slice(&frame.samples[source_offset..source_offset + count]);
            state.partial_scalars += count;
            source_offset += count;

            if state.partial_scalars == OPUS_FRAME_SCALARS {
                let start_sample = state
                    .partial_start_sample
                    .take()
                    .unwrap_or(frame.start_sample);
                Self::encode_and_send(
                    state
                        .encoder
                        .as_mut()
                        .ok_or_else(|| OutputError::new("Opus encoder is not initialized"))?,
                    &state.partial,
                    OPUS_FRAME_SAMPLES,
                    OPUS_FRAME_SAMPLES,
                    start_sample,
                    sink,
                    cancellation,
                )?;
                state.partial_scalars = 0;
            }
        }

        state.next_input_sample = Some(
            frame
                .start_sample
                .saturating_add(u64::try_from(frame.samples_per_channel).unwrap_or(u64::MAX)),
        );
        Ok(())
    }
}

fn duration_to_samples(position: Duration) -> Result<u64, OutputError> {
    let samples = position.as_nanos() * u128::from(OUTPUT_SAMPLE_RATE_HZ) / 1_000_000_000;
    u64::try_from(samples).map_err(|_| OutputError::new("seek position exceeds the PCM timeline"))
}

fn duration_from_samples(samples: u64) -> Duration {
    let rate = u64::from(OUTPUT_SAMPLE_RATE_HZ);
    let seconds = samples / rate;
    let remainder = samples % rate;
    let nanoseconds = (u128::from(remainder) * 1_000_000_000 / u128::from(rate)) as u32;
    Duration::new(seconds, nanoseconds)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;

mod pcm_output;

mod encoded_output;
