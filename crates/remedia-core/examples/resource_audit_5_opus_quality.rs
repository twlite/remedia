use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opus2::{Channels, Decoder};
use remedia_core::{
    CancellationToken, EncodedOpusPacket, InputSource, OpusEncoderConfig, OpusEncoderOutput,
    OpusPacketSink, OutputError, PlaybackSession, PlaybackState,
};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const FRAME_SAMPLES: usize = 960;
const LOOKAHEAD_SAMPLES: usize = 312;
const QUALITY_SEGMENT_SAMPLES: usize = SAMPLE_RATE as usize / 10;

#[derive(Clone, Default)]
struct CaptureSink(Arc<Mutex<Capture>>);

#[derive(Default)]
struct Capture {
    packets: Vec<EncodedOpusPacket>,
    finished: bool,
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

        lock(&self.0).packets.push(packet);
        Ok(())
    }

    fn flush(
        &self,
        _position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            Err(OutputError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        if cancellation.is_cancelled() {
            return Err(OutputError::Cancelled);
        }

        lock(&self.0).finished = true;
        Ok(())
    }

    fn cancel(&self) {}
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments
        .next()
        .map(std::path::PathBuf::from)
        .ok_or("usage: resource_audit_5_opus_quality FILE [COMPLEXITY]")?;
    let complexity = arguments
        .next()
        .map(|value| value.to_string_lossy().parse())
        .transpose()?
        .unwrap_or(9);
    if arguments.next().is_some() {
        return Err("usage: resource_audit_5_opus_quality FILE [COMPLEXITY]".into());
    }

    let reference = read_pcm_wav(&path)?;
    let sink = CaptureSink::default();
    let packets = Arc::clone(&sink.0);
    let output = OpusEncoderOutput::new_with_config(
        sink,
        OpusEncoderConfig {
            complexity,
            ..OpusEncoderConfig::default()
        },
    )?;
    let session = PlaybackSession::new();
    let info = session.play(InputSource::file(path), output)?;
    let state = session.wait();
    if state != PlaybackState::Completed {
        return Err(session
            .last_error()
            .map(|error| Box::new(error) as Box<dyn Error>)
            .unwrap_or_else(|| format!("playback ended in {state:?}").into()));
    }
    session.stop()?;

    let captured = lock(&packets);
    if !captured.finished {
        return Err("packet sink did not finish cleanly".into());
    }
    let packet_count = captured.packets.len();
    let encoded_samples = captured
        .packets
        .iter()
        .map(|packet| u64::from(packet.samples_per_channel))
        .sum::<u64>();
    let media_samples = captured
        .packets
        .iter()
        .map(|packet| u64::from(packet.media_samples_per_channel))
        .sum::<u64>();

    let mut decoder = Decoder::new(SAMPLE_RATE, Channels::Stereo)?;
    let mut decode_buffer = vec![0.0_f32; 5_760 * CHANNELS];
    let mut decoded = Vec::with_capacity(encoded_samples as usize * CHANNELS);
    for packet in &captured.packets {
        let samples = decoder.decode_float(&packet.bytes, &mut decode_buffer, false)?;
        decoded.extend_from_slice(&decode_buffer[..samples * CHANNELS]);
    }
    drop(captured);

    let (snr, segment_snr) = quality_metrics(&reference, &decoded)?;
    let percentile_index = ((segment_snr.len() - 1) as f64 * 0.10).floor() as usize;
    let mut sorted_segments = segment_snr;
    sorted_segments.sort_by(f64::total_cmp);
    let segment_snr_p10 = sorted_segments[percentile_index];
    let segment_snr_min = sorted_segments[0];
    let expected_media_samples = reference.len() as u64 / CHANNELS as u64;
    if media_samples != expected_media_samples {
        return Err(format!(
            "packet media total {media_samples} differs from input {expected_media_samples}"
        )
        .into());
    }
    if encoded_samples != packet_count as u64 * FRAME_SAMPLES as u64 {
        return Err("encoder output did not contain 20 ms packets".into());
    }

    println!(
        "{}",
        serde_json::json!({
            "complexity": complexity,
            "bitrate": 128_000,
            "vbr": true,
            "input_frames": expected_media_samples,
            "packet_count": packet_count,
            "encoded_samples": encoded_samples,
            "media_samples": media_samples,
            "decoder_samples": decoded.len() / CHANNELS,
            "encoder_lookahead_samples": LOOKAHEAD_SAMPLES,
            "compared_frames": expected_media_samples.saturating_sub(LOOKAHEAD_SAMPLES as u64),
            "snr_db": snr,
            "segment_snr_p10_db": segment_snr_p10,
            "segment_snr_min_db": segment_snr_min,
            "segment_frames": sorted_segments.len(),
            "quality_metric": "sample-domain SNR after aligning the 312-sample encoder lookahead",
            "quality_limit": "synthetic deterministic music-like PCM; objective SNR does not establish subjective quality or performance on program material",
            "source_duration_seconds": info.duration.map(|duration| duration.as_secs_f64()),
        })
    );

    Ok(())
}

fn read_pcm_wav(path: &Path) -> Result<Vec<f32>, Box<dyn Error>> {
    let bytes = fs::read(path)?;
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("quality fixture must be a RIFF/WAVE file".into());
    }

    let (mut format, mut data) = (None, None);
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let chunk_size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize;
        let start = offset + 8;
        let end = start
            .checked_add(chunk_size)
            .filter(|end| *end <= bytes.len())
            .ok_or("WAV chunk exceeds the file size")?;
        match &bytes[offset..offset + 4] {
            b"fmt " if chunk_size >= 16 => format = Some(&bytes[start..end]),
            b"data" => data = Some(&bytes[start..end]),
            _ => {}
        }
        offset = end + (chunk_size & 1);
    }

    let format = format.ok_or("WAV format chunk is missing")?;
    let data = data.ok_or("WAV audio data chunk is missing")?;
    let encoding = u16::from_le_bytes(format[0..2].try_into()?);
    let channels = u16::from_le_bytes(format[2..4].try_into()?);
    let sample_rate = u32::from_le_bytes(format[4..8].try_into()?);
    let bits_per_sample = u16::from_le_bytes(format[14..16].try_into()?);
    if encoding != 1 || channels != 2 || sample_rate != SAMPLE_RATE || bits_per_sample != 16 {
        return Err("quality fixture must be 48 kHz stereo signed 16-bit PCM".into());
    }
    if !data.len().is_multiple_of(2) {
        return Err("PCM data has an incomplete sample".into());
    }

    Ok(data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|bytes| i16::from_le_bytes(*bytes) as f32 / 32_768.0)
        .collect())
}

fn quality_metrics(reference: &[f32], decoded: &[f32]) -> Result<(f64, Vec<f64>), Box<dyn Error>> {
    let reference_frames = reference.len() / CHANNELS;
    let decoded_frames = decoded.len() / CHANNELS;
    let compared_frames = reference_frames.saturating_sub(LOOKAHEAD_SAMPLES);
    if !reference.len().is_multiple_of(CHANNELS)
        || !decoded.len().is_multiple_of(CHANNELS)
        || decoded_frames < compared_frames + LOOKAHEAD_SAMPLES
    {
        return Err("decoded Opus output is too short to align with the input".into());
    }

    let mut signal_power = 0.0;
    let mut error_power = 0.0;
    let mut segments = Vec::new();
    for segment_start in (0..compared_frames).step_by(QUALITY_SEGMENT_SAMPLES) {
        let segment_end = (segment_start + QUALITY_SEGMENT_SAMPLES).min(compared_frames);
        let mut segment_signal = 0.0;
        let mut segment_error = 0.0;
        for frame in segment_start..segment_end {
            for channel in 0..CHANNELS {
                let expected = f64::from(reference[frame * CHANNELS + channel]);
                let actual = f64::from(decoded[(frame + LOOKAHEAD_SAMPLES) * CHANNELS + channel]);
                let error = actual - expected;
                signal_power += expected * expected;
                error_power += error * error;
                segment_signal += expected * expected;
                segment_error += error * error;
            }
        }
        if segment_signal > 1e-10 {
            segments.push(snr_db(segment_signal, segment_error));
        }
    }
    if segments.is_empty() || signal_power == 0.0 || error_power == 0.0 {
        return Err("quality metrics could not be calculated for this fixture".into());
    }

    Ok((snr_db(signal_power, error_power), segments))
}

fn snr_db(signal_power: f64, error_power: f64) -> f64 {
    10.0 * (signal_power / error_power).log10()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
