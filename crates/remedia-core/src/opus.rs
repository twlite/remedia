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

impl MediaOutput for OpusEncoderOutput {
    fn write_frame(
        &self,
        frame: PcmFrame,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        let mut state = lock(&self.state);
        self.initialize_encoder(&mut state)?;
        Self::write_pcm(&mut state, frame, &*self.sink, cancellation)
    }

    fn encoded_opus_output(&self) -> Option<&dyn EncodedOpusOutput> {
        Some(self)
    }

    fn flush(
        &self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.flush_samples(duration_to_samples(position)?, cancellation)
    }

    fn flush_samples(
        &self,
        sample_position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        self.sink.flush_samples(sample_position, cancellation)?;
        let mut state = lock(&self.state);
        if let Some(encoder) = state.encoder.as_mut() {
            encoder.reset_state().map_err(OutputError::with_source)?;
        }
        state.partial_scalars = 0;
        state.partial_start_sample = None;
        state.next_input_sample = Some(sample_position);
        state.finished = false;
        Ok(())
    }

    fn filter_transition(
        &self,
        next_sample_position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        let mut state = lock(&self.state);
        if let Some(expected) = state.next_input_sample
            && expected != next_sample_position
        {
            tracing::error!(
                name: "core.opus.filter_transition_discontinuity",
                target: "remedia_core::opus",
                expected_sample = expected,
                received_sample = next_sample_position,
                "filter transition did not match the next Opus PCM sample"
            );
            return Err(OutputError::new(format!(
                "filter transition expected sample {expected}, received {next_sample_position}"
            )));
        }
        // A partial Opus frame is already accepted PCM. Keep it so it can be
        // completed by the next graph's contiguous samples. This avoids a seek
        // flush, encoder reset, and Discord RTP generation change.
        state.next_input_sample = Some(next_sample_position);
        Ok(())
    }

    fn pause(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        self.sink.pause(cancellation)
    }

    fn resume(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        self.sink.resume(cancellation)
    }

    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        let mut state = lock(&self.state);
        if state.finished {
            return Err(OutputError::new("Opus output finished more than once"));
        }
        if state.partial_scalars != 0 {
            let media_samples = state.partial_scalars / 2;
            let partial_scalars = state.partial_scalars;
            state.partial[partial_scalars..].fill(0.0);
            let start_sample = state.partial_start_sample.take().unwrap_or(0);
            with_dispatch(&self.observability, || {
                tracing::debug!(
                    name: "core.opus.final_frame_padded",
                    target: "remedia_core::opus",
                    media_samples_per_channel = media_samples,
                    encoded_samples_per_channel = OPUS_FRAME_SAMPLES,
                    start_sample,
                    "final partial PCM frame padded for Opus encoding"
                );
            });
            let padded = state.partial;
            let encoder = state
                .encoder
                .as_mut()
                .ok_or_else(|| OutputError::new("Opus encoder is not initialized"))?;
            Self::encode_and_send(
                encoder,
                &padded,
                OPUS_FRAME_SAMPLES,
                media_samples,
                start_sample,
                &*self.sink,
                cancellation,
            )?;
            state.partial_scalars = 0;
        }
        self.sink.finish(cancellation)?;
        state.finished = true;
        Ok(())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.sink.cancel();
        with_dispatch(&self.observability, || {
            tracing::debug!(
                name: "core.opus.output_cancelled",
                target: "remedia_core::opus",
                "Opus output cancelled"
            );
        });
    }

    fn played_position(&self) -> Option<Duration> {
        self.sink.played_position()
    }
}

impl EncodedOpusOutput for OpusEncoderOutput {
    fn supports_encoded_packet(&self, packet: &EncodedOpusPacket) -> bool {
        matches!(
            packet.samples_per_channel,
            120 | 240 | 480 | 960 | 1_920 | 2_880 | 5_760
        ) && packet.media_samples_per_channel == packet.samples_per_channel
            && !packet.bytes.is_empty()
            && self.sink.supports_passthrough_packet(packet)
    }

    fn write_encoded_packet(
        &self,
        packet: EncodedOpusPacket,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        if !self.supports_encoded_packet(&packet) {
            return Err(OutputError::new(
                "Opus packet is not compatible with this output sink",
            ));
        }
        let mut state = lock(&self.state);
        if state.finished {
            return Err(OutputError::new(
                "Opus packet written after output finished",
            ));
        }
        if state.partial_scalars != 0 {
            return Err(OutputError::new(
                "cannot pass through Opus while a partial PCM frame is pending",
            ));
        }
        if let Some(expected) = state.next_input_sample
            && expected != packet.start_sample
        {
            tracing::error!(
                name: "core.opus.packet_discontinuity",
                target: "remedia_core::opus",
                expected_sample = expected,
                received_sample = packet.start_sample,
                "Opus output rejected a discontinuous encoded packet timeline"
            );
            return Err(OutputError::new(format!(
                "Opus timeline is discontinuous: expected sample {expected}, received {}",
                packet.start_sample
            )));
        }
        let end_sample = packet
            .start_sample
            .saturating_add(u64::from(packet.media_samples_per_channel));
        self.sink.send_packet(packet, cancellation)?;
        state.next_input_sample = Some(end_sample);
        Ok(())
    }

    fn prepare_passthrough(
        &self,
        next_sample_position: u64,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        let mut state = lock(&self.state);
        if let Some(expected) = state.next_input_sample
            && expected != next_sample_position
        {
            return Err(OutputError::new(format!(
                "passthrough transition expected sample {expected}, received {next_sample_position}"
            )));
        }
        if state.partial_scalars != 0 {
            let media_samples = state.partial_scalars / 2;
            let encoded_samples = [120, 240, 480, OPUS_FRAME_SAMPLES]
                .into_iter()
                .find(|samples| *samples >= media_samples)
                .ok_or_else(|| OutputError::new("partial Opus frame is too large to flush"))?;
            let encoded_scalars = encoded_samples * 2;
            let partial_scalars = state.partial_scalars;
            state.partial[partial_scalars..encoded_scalars].fill(0.0);
            let start_sample = state.partial_start_sample.unwrap_or(next_sample_position);
            let pcm = state.partial;
            let encoder = state
                .encoder
                .as_mut()
                .ok_or_else(|| OutputError::new("Opus encoder is not initialized"))?;
            Self::encode_and_send(
                encoder,
                &pcm[..encoded_scalars],
                encoded_samples,
                media_samples,
                start_sample,
                &*self.sink,
                cancellation,
            )?;
            state.partial_scalars = 0;
            state.partial_start_sample = None;
        }
        if let Some(encoder) = state.encoder.as_mut() {
            encoder.reset_state().map_err(OutputError::with_source)?;
        }
        state.next_input_sample = Some(next_sample_position);
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
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct CaptureSink(Arc<Mutex<CaptureState>>);

    #[derive(Default)]
    struct CaptureState {
        packets: Vec<EncodedOpusPacket>,
        flushes: usize,
        flush_sample_positions: Vec<u64>,
        finished: bool,
        fail: bool,
    }

    impl OpusPacketSink for CaptureSink {
        fn send_packet(
            &self,
            packet: EncodedOpusPacket,
            cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            if cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            let mut state = lock(&self.0);
            if state.fail {
                return Err(OutputError::new("capture sink failure"));
            }
            state.packets.push(packet);
            Ok(())
        }

        fn flush(
            &self,
            _position: Duration,
            _cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            lock(&self.0).flushes += 1;
            Ok(())
        }

        fn flush_samples(
            &self,
            sample_position: u64,
            _cancellation: &CancellationToken,
        ) -> Result<(), OutputError> {
            let mut state = lock(&self.0);
            state.flushes += 1;
            state.flush_sample_positions.push(sample_position);
            Ok(())
        }

        fn finish(&self, _cancellation: &CancellationToken) -> Result<(), OutputError> {
            lock(&self.0).finished = true;
            Ok(())
        }

        fn cancel(&self) {}
    }

    fn pcm(start_sample: u64, samples_per_channel: usize) -> PcmFrame {
        PcmFrame {
            samples: vec![0.0; samples_per_channel * 2],
            samples_per_channel,
            start_sample,
        }
    }

    fn assert_settings(output: &OpusEncoderOutput, config: OpusEncoderConfig) {
        let mut state = lock(&output.state);
        let encoder = state.encoder.as_mut().expect("PCM initialized encoder");
        assert_eq!(
            encoder.get_bitrate().unwrap(),
            Bitrate::Bits(config.bitrate)
        );
        assert_eq!(encoder.get_complexity().unwrap(), config.complexity);
        assert_eq!(encoder.get_vbr().unwrap(), config.vbr);
        assert_eq!(encoder.get_application().unwrap(), Application::Audio);
        assert_eq!(encoder.get_sample_rate().unwrap(), OUTPUT_SAMPLE_RATE_HZ);
    }

    #[test]
    fn defaults_and_custom_settings_reach_native_controls_and_survive_resets() {
        for config in [
            OpusEncoderConfig::default(),
            OpusEncoderConfig {
                bitrate: 64_000,
                complexity: 3,
                vbr: false,
            },
            OpusEncoderConfig {
                bitrate: 500,
                complexity: 0,
                vbr: true,
            },
            OpusEncoderConfig {
                bitrate: 512_000,
                complexity: 10,
                vbr: false,
            },
        ] {
            let output =
                OpusEncoderOutput::new_with_config(CaptureSink::default(), config).unwrap();
            assert!(lock(&output.state).encoder.is_none());
            let cancellation = CancellationToken::new();
            output.write_frame(pcm(0, 960), &cancellation).unwrap();
            assert_settings(&output, config);
            output.flush_samples(4_800, &cancellation).unwrap();
            output.write_frame(pcm(4_800, 960), &cancellation).unwrap();
            assert_settings(&output, config);
            output.finish(&cancellation).unwrap();
        }
        let output = OpusEncoderOutput::new(CaptureSink::default()).unwrap();
        output
            .write_frame(pcm(0, 960), &CancellationToken::new())
            .unwrap();
        assert_settings(&output, OpusEncoderConfig::default());
    }

    #[test]
    fn invalid_settings_fail_before_native_initialization() {
        for bitrate in [i32::MIN, -1, 0, 499, 512_001, i32::MAX] {
            let config = OpusEncoderConfig {
                bitrate,
                ..Default::default()
            };
            assert!(OpusEncoderOutput::new_with_config(CaptureSink::default(), config).is_err());
        }
        for complexity in [-1, 11, i32::MAX] {
            let config = OpusEncoderConfig {
                complexity,
                ..Default::default()
            };
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("complexity")
            );
        }
    }

    #[test]
    fn concurrent_outputs_have_independent_native_encoders() {
        let configs = [
            OpusEncoderConfig::default(),
            OpusEncoderConfig {
                bitrate: 96_000,
                complexity: 2,
                vbr: false,
            },
        ];
        let outputs = configs.map(|config| {
            OpusEncoderOutput::new_with_config(CaptureSink::default(), config).unwrap()
        });
        std::thread::scope(|scope| {
            for output in &outputs {
                scope.spawn(move || {
                    output
                        .write_frame(pcm(0, 960), &CancellationToken::new())
                        .unwrap();
                });
            }
        });
        outputs[0]
            .flush_samples(0, &CancellationToken::new())
            .unwrap();
        for (output, config) in outputs.iter().zip(configs) {
            assert_settings(output, config);
        }
    }

    #[test]
    fn encoded_packet_delivery_keeps_libopus_encoder_uninitialized() {
        let sink = CaptureSink::default();
        let state = Arc::clone(&sink.0);
        let output = OpusEncoderOutput::new_with_config(
            sink,
            OpusEncoderConfig {
                bitrate: 64_000,
                complexity: 1,
                vbr: false,
            },
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        assert!(lock(&output.state).encoder.is_none());

        EncodedOpusOutput::write_encoded_packet(
            &output,
            EncodedOpusPacket {
                bytes: vec![0xF8, 0xFF, 0xFE],
                samples_per_channel: 960,
                media_samples_per_channel: 960,
                start_sample: 0,
            },
            &cancellation,
        )
        .unwrap();

        assert!(lock(&output.state).encoder.is_none());
        let packets = &lock(&state).packets;
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].bytes, [0xF8, 0xFF, 0xFE]);
    }

    #[test]
    fn encodes_complete_stereo_frames_and_keeps_timeline_metadata() {
        let sink = CaptureSink::default();
        let state = Arc::clone(&sink.0);
        let output = OpusEncoderOutput::new(sink).unwrap();
        let cancellation = CancellationToken::new();

        output.write_frame(pcm(0, 400), &cancellation).unwrap();
        assert!(lock(&state).packets.is_empty());
        output.write_frame(pcm(400, 560), &cancellation).unwrap();
        let packets = &lock(&state).packets;
        assert_eq!(packets.len(), 1);
        assert!(!packets[0].bytes.is_empty());
        assert_eq!(packets[0].samples_per_channel, 960);
        assert_eq!(packets[0].media_samples_per_channel, 960);
        assert_eq!(packets[0].start_sample, 0);
    }

    #[test]
    fn finish_pads_one_partial_frame_and_reports_both_durations() {
        let sink = CaptureSink::default();
        let state = Arc::clone(&sink.0);
        let output = OpusEncoderOutput::new(sink).unwrap();
        let cancellation = CancellationToken::new();

        output.write_frame(pcm(120, 240), &cancellation).unwrap();
        output.finish(&cancellation).unwrap();
        let state = lock(&state);
        assert!(state.finished);
        assert_eq!(state.packets.len(), 1);
        assert_eq!(state.packets[0].samples_per_channel, 960);
        assert_eq!(state.packets[0].media_samples_per_channel, 240);
        assert_eq!(state.packets[0].start_sample, 120);
    }

    #[test]
    fn sink_errors_propagate_from_packet_submission() {
        let sink = CaptureSink::default();
        lock(&sink.0).fail = true;
        let output = OpusEncoderOutput::new(sink).unwrap();
        let error = output
            .write_frame(pcm(0, 960), &CancellationToken::new())
            .unwrap_err();
        assert!(error.to_string().contains("capture sink failure"));
    }

    #[test]
    fn flush_discards_partial_pcm_and_restarts_encoder_at_seek_position() {
        let sink = CaptureSink::default();
        let state = Arc::clone(&sink.0);
        let output = OpusEncoderOutput::new(sink).unwrap();
        let cancellation = CancellationToken::new();

        output.write_frame(pcm(0, 240), &cancellation).unwrap();
        output
            .flush(Duration::from_millis(100), &cancellation)
            .unwrap();
        output.write_frame(pcm(4_800, 960), &cancellation).unwrap();
        let packets = &lock(&state).packets;
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].start_sample, 4_800);
    }

    #[test]
    fn sample_accurate_flush_preserves_non_round_duration_boundaries() {
        for sample_position in [1_024, 1_337, 2_003, 4_096, 10_001] {
            let sink = CaptureSink::default();
            let state = Arc::clone(&sink.0);
            let output = OpusEncoderOutput::new(sink).unwrap();
            let cancellation = CancellationToken::new();
            output
                .write_frame(pcm(0, sample_position as usize), &cancellation)
                .unwrap();
            output
                .flush_samples(sample_position, &cancellation)
                .unwrap();
            assert!(
                output
                    .write_frame(pcm(sample_position, 960), &cancellation)
                    .is_ok()
            );
            assert_eq!(
                lock(&state).flush_sample_positions,
                [sample_position],
                "the exact sample position reaches the packet sink"
            );
        }
    }

    #[test]
    fn live_filter_transition_keeps_partial_opus_pcm_and_does_not_flush_sink() {
        let sink = CaptureSink::default();
        let state = Arc::clone(&sink.0);
        let output = OpusEncoderOutput::new(sink).unwrap();
        let cancellation = CancellationToken::new();

        output.write_frame(pcm(0, 1_024), &cancellation).unwrap();
        output.filter_transition(1_024, &cancellation).unwrap();
        output
            .write_frame(pcm(1_024, 1_024), &cancellation)
            .unwrap();
        output.finish(&cancellation).unwrap();

        let state = lock(&state);
        assert_eq!(state.packets.len(), 3);
        assert_eq!(state.packets[0].start_sample, 0);
        assert_eq!(state.packets[1].start_sample, 960);
        assert_eq!(state.packets[2].start_sample, 1_920);
        assert_eq!(state.packets[1].media_samples_per_channel, 960);
        assert_eq!(state.packets[2].media_samples_per_channel, 128);
        assert_eq!(state.flushes, 0, "filter changes must not flush the sink");
        assert!(state.finished);
    }
}
