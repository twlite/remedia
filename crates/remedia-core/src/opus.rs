use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use opus2::{Application, Channels, Encoder};
use remedia_observability::ObservabilityHandle;

use crate::decoder::{OUTPUT_SAMPLE_RATE_HZ, PcmFrame};
use crate::output::{MediaOutput, OutputError};
use crate::source::CancellationToken;
use crate::with_dispatch;

const OPUS_FRAME_SAMPLES: usize = 960;
const OPUS_FRAME_SCALARS: usize = OPUS_FRAME_SAMPLES * 2;
const MAX_OPUS_PACKET_BYTES: usize = 4_000;

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

    /// Discard packets queued before a seek and move the media timeline.
    fn flush(
        &self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError>;

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
/// It owns one native libopus encoder and one packet sink. At most one partial
/// frame (under 20 ms) is retained. A short final frame is zero-padded to 20 ms;
/// the packet records both the 20 ms encoded duration and its shorter media
/// duration. Seeking discards partial PCM and resets the encoder state.
pub struct OpusEncoderOutput {
    state: Mutex<EncoderState>,
    sink: Box<dyn OpusPacketSink>,
    cancelled: AtomicBool,
    observability: ObservabilityHandle,
}

struct EncoderState {
    encoder: Encoder,
    partial: [f32; OPUS_FRAME_SCALARS],
    partial_scalars: usize,
    partial_start_sample: Option<u64>,
    next_input_sample: Option<u64>,
    finished: bool,
}

impl OpusEncoderOutput {
    /// Create the native stereo encoder and attach an owned packet sink.
    pub fn new(sink: impl OpusPacketSink) -> Result<Self, OutputError> {
        Self::new_with_observability(sink, ObservabilityHandle::default())
    }

    /// Create the encoder with an application-owned local diagnostic context.
    pub fn new_with_observability(
        sink: impl OpusPacketSink,
        observability: ObservabilityHandle,
    ) -> Result<Self, OutputError> {
        let encoder =
            match Encoder::new(OUTPUT_SAMPLE_RATE_HZ, Channels::Stereo, Application::Audio) {
                Ok(encoder) => encoder,
                Err(error) => {
                    with_dispatch(&observability, || {
                        tracing::error!(
                            name: "core.opus.encoder_initialization_failed",
                            target: "remedia_core::opus",
                            error_kind = "native_encoder",
                            "libopus encoder initialization failed"
                        );
                    });
                    return Err(OutputError::with_source(error));
                }
            };

        with_dispatch(&observability, || {
            tracing::info!(
                name: "core.opus.encoder_initialized",
                target: "remedia_core::opus",
                sample_rate_hz = OUTPUT_SAMPLE_RATE_HZ,
                channels = 2_u16,
                frame_samples_per_channel = OPUS_FRAME_SAMPLES,
                "libopus encoder initialized"
            );
        });

        Ok(Self {
            state: Mutex::new(EncoderState {
                encoder,
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
                samples_per_channel: OPUS_FRAME_SAMPLES as u32,
                media_samples_per_channel,
                start_sample,
            },
            cancellation,
        )?;
        tracing::trace!(
            name: "core.opus.packet_encoded",
            target: "remedia_core::opus",
            encoded_samples_per_channel = OPUS_FRAME_SAMPLES,
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
                    &mut state.encoder,
                    &frame.samples[source_offset..source_offset + OPUS_FRAME_SCALARS],
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
                    &mut state.encoder,
                    &state.partial,
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
        Self::write_pcm(&mut state, frame, &*self.sink, cancellation)
    }

    fn flush(
        &self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.ensure_active(cancellation)?;
        self.sink.flush(position, cancellation)?;
        let mut state = lock(&self.state);
        state
            .encoder
            .reset_state()
            .map_err(OutputError::with_source)?;
        state.partial_scalars = 0;
        state.partial_start_sample = None;
        state.next_input_sample = Some(duration_to_samples(position)?);
        state.finished = false;
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
            Self::encode_and_send(
                &mut state.encoder,
                &padded,
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

fn duration_to_samples(position: Duration) -> Result<u64, OutputError> {
    let samples = position.as_nanos() * u128::from(OUTPUT_SAMPLE_RATE_HZ) / 1_000_000_000;
    u64::try_from(samples).map_err(|_| OutputError::new("seek position exceeds the PCM timeline"))
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
}
