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
/// counts source audio represented by the packet. It is shorter than
/// `samples_per_channel` for the final zero-padded frame, for the first frame
/// after a seek that begins with under 20 ms of leading silence, and for a forwarded
/// source packet whose first samples are decoder pre-roll (Opus pre-skip). Those
/// samples count toward RTP time but not media time. `start_sample` is the start on
/// the normalized 48 kHz media timeline, and successive packets are contiguous in
/// media samples.
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
    /// Opt in to readiness-driven packet admission and resumable transport barriers.
    /// All poll methods must be nonblocking; legacy sinks retain dedicated execution.
    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_nonblocking(&self) -> bool {
        false
    }

    /// Accept the packet only on success. Pending and errors retain the original allocation
    /// in `packet`; a pending poll must register the waker before checking capacity.
    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_packet(
        &self,
        _packet: &mut Option<EncodedOpusPacket>,
        _cx: &mut std::task::Context<'_>,
        _cancellation: &CancellationToken,
    ) -> std::task::Poll<Result<(), OutputError>> {
        std::task::Poll::Ready(Err(OutputError::new(
            "sink has no packet readiness capability",
        )))
    }

    /// Poll a transport barrier after all earlier packets have been admitted. The operation
    /// must be started once and acknowledged before returning success.
    #[cfg(feature = "experimental-scheduler")]
    fn scheduler_poll_control(
        &self,
        _control: crate::OutputControl,
        _cx: &mut std::task::Context<'_>,
        _cancellation: &CancellationToken,
    ) -> std::task::Poll<Result<(), OutputError>> {
        std::task::Poll::Ready(Err(OutputError::new(
            "sink has no barrier readiness capability",
        )))
    }

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
    sink: packet_buffer::PacketBuffer,
    #[cfg(feature = "experimental-scheduler")]
    pending_pcm: Mutex<Option<(PcmFrame, usize)>>,
    cancelled: AtomicBool,
    observability: ObservabilityHandle,
}

struct EncoderState {
    encoder: Option<Encoder>,
    partial: [f32; OPUS_FRAME_SCALARS],
    partial_scalars: usize,
    partial_start_sample: Option<u64>,
    /// Leading silence (per channel) that `align_encoder_frames` placed at the start of the
    /// partial frame. It is encoded but is not media.
    partial_lead_samples: usize,
    /// Frames still to encode without producing a packet, because their audio was already played
    /// as source packets (see `begin_seamless_pcm`).
    suppressed_frames: u32,
    /// Live PCM has been fed since `begin_seamless_pcm`, but no packet has been sent yet, so the
    /// client has not heard any of it.
    primed_unflushed: bool,
    /// Samples subtracted from a sent packet's `start_sample`. After `begin_seamless_pcm` the
    /// encoder's output lags its input by its lookahead, so a packet's heard position is that
    /// many samples before its input position; this keeps the accounting in heard media.
    start_bias: u64,
    /// How many live samples per channel were fed since `begin_seamless_pcm`.
    primed_live_samples: usize,
    /// The partial frame holds only that alignment silence; no real PCM has followed it yet.
    partial_is_alignment_only: bool,
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
                partial_lead_samples: 0,
                suppressed_frames: 0,
                primed_unflushed: false,
                start_bias: 0,
                primed_live_samples: 0,
                partial_is_alignment_only: false,
                next_input_sample: None,
                finished: false,
            }),
            sink: packet_buffer::PacketBuffer::new(Box::new(sink)),
            #[cfg(feature = "experimental-scheduler")]
            pending_pcm: Mutex::new(None),
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

    /// Encode one frame and hand it to the sink, unless `suppress` is set. A suppressed frame is
    /// still encoded, so the encoder's state advances, but its audio has already been played as
    /// source packets and no packet is produced.
    #[allow(clippy::too_many_arguments)]
    fn encode_and_send(
        encoder: &mut Encoder,
        pcm: &[f32],
        encoded_samples_per_channel: usize,
        media_samples_per_channel: usize,
        start_sample: u64,
        suppress: bool,
        sink: &dyn OpusPacketSink,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        // Encode into bounded scratch storage. Only the encoded payload is owned
        // by the sink; retaining a 4 KB allocation per queued packet wastes memory.
        let mut scratch = [0; MAX_OPUS_PACKET_BYTES];
        let len = encoder.encode_float(pcm, &mut scratch).map_err(|error| {
            tracing::error!(
                name: "core.opus.encode_failed",
                target: "remedia_core::opus",
                error_kind = "native_encode",
                "libopus could not encode a PCM frame"
            );
            OutputError::with_source(error)
        })?;
        if suppress {
            return Ok(());
        }
        let bytes = scratch[..len].to_vec();
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
        samples: &[f32],
        start_sample: u64,
        sink: &dyn OpusPacketSink,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        let scalar_count = samples.len();
        if !scalar_count.is_multiple_of(2) {
            return Err(OutputError::new("PCM frame must contain stereo samples"));
        }
        if state.finished {
            return Err(OutputError::new("PCM frame written after output finished"));
        }
        if scalar_count != 0 {
            state.partial_is_alignment_only = false;
        }
        if state.primed_unflushed {
            state.primed_live_samples += scalar_count / 2;
        }
        if let Some(expected) = state.next_input_sample
            && expected != start_sample
        {
            tracing::error!(
                name: "core.opus.pcm_discontinuity",
                target: "remedia_core::opus",
                expected_sample = expected,
                received_sample = start_sample,
                "Opus output rejected a discontinuous PCM timeline"
            );
            return Err(OutputError::new(format!(
                "PCM timeline is discontinuous: expected sample {expected}, received {}",
                start_sample
            )));
        }

        let mut source_offset = 0;
        while source_offset < scalar_count {
            if state.partial_scalars == 0 {
                state.partial_start_sample =
                    Some(start_sample + u64::try_from(source_offset / 2).unwrap_or(u64::MAX));
            }

            if state.partial_scalars == 0 && scalar_count - source_offset >= OPUS_FRAME_SCALARS {
                let start_sample = state.partial_start_sample.take().unwrap_or(start_sample);
                let suppress = state.suppressed_frames > 0;
                let start_sample = start_sample.saturating_sub(state.start_bias);
                Self::encode_and_send(
                    state
                        .encoder
                        .as_mut()
                        .ok_or_else(|| OutputError::new("Opus encoder is not initialized"))?,
                    &samples[source_offset..source_offset + OPUS_FRAME_SCALARS],
                    OPUS_FRAME_SAMPLES,
                    OPUS_FRAME_SAMPLES,
                    start_sample,
                    suppress,
                    sink,
                    cancellation,
                )?;
                state.suppressed_frames = state.suppressed_frames.saturating_sub(1);
                state.primed_unflushed &= suppress;
                source_offset += OPUS_FRAME_SCALARS;
                continue;
            }

            let count =
                (OPUS_FRAME_SCALARS - state.partial_scalars).min(scalar_count - source_offset);
            state.partial[state.partial_scalars..state.partial_scalars + count]
                .copy_from_slice(&samples[source_offset..source_offset + count]);
            state.partial_scalars += count;
            source_offset += count;

            if state.partial_scalars == OPUS_FRAME_SCALARS {
                let start_sample = state.partial_start_sample.take().unwrap_or(start_sample);
                let suppress = state.suppressed_frames > 0;
                let start_sample = start_sample.saturating_sub(state.start_bias);
                Self::encode_and_send(
                    state
                        .encoder
                        .as_mut()
                        .ok_or_else(|| OutputError::new("Opus encoder is not initialized"))?,
                    &state.partial,
                    OPUS_FRAME_SAMPLES,
                    OPUS_FRAME_SAMPLES - state.partial_lead_samples,
                    start_sample,
                    suppress,
                    sink,
                    cancellation,
                )?;
                state.suppressed_frames = state.suppressed_frames.saturating_sub(1);
                state.primed_unflushed &= suppress;
                state.partial_scalars = 0;
                state.partial_lead_samples = 0;
            }
        }

        state.next_input_sample =
            Some(start_sample.saturating_add(u64::try_from(scalar_count / 2).unwrap_or(u64::MAX)));
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

impl EncoderState {
    /// Drop grid-alignment silence that no real audio ever followed.
    fn discard_alignment_padding(&mut self) {
        if self.partial_is_alignment_only {
            self.partial_scalars = 0;
            self.partial_start_sample = None;
            self.partial_lead_samples = 0;
            self.partial_is_alignment_only = false;
        }
    }
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

mod packet_buffer;

#[cfg(feature = "experimental-scheduler")]
mod readiness;

#[cfg(all(test, feature = "experimental-scheduler"))]
mod tests_readiness;
