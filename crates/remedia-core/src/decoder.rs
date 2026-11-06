use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use ffmpeg::codec::decoder::Audio as AudioCodecDecoder;
use ffmpeg::software::resampling::Context as Resampler;
use ffmpeg::util::format::sample::Type as SampleType;
use ffmpeg::{ChannelLayout, Rational};
use ffmpeg_next as ffmpeg;

use crate::error::{DecoderError, ffmpeg_error};
use crate::input::{self, OpenedInput};
use crate::source::{CancellationToken, InputSource};

pub(crate) const OUTPUT_SAMPLE_RATE_HZ: u32 = 48_000;
const OUTPUT_CHANNELS: u16 = 2;
const OUTPUT_SAMPLE_FORMAT: ffmpeg::format::Sample =
    ffmpeg::format::Sample::F32(SampleType::Planar);
const RESAMPLER_PADDING_SAMPLES: usize = 128;
const OPUS_PREROLL_SAMPLES: u64 = 5_760;
const OPUS_HISTORY_MAX_BYTES: usize = 512 * 1024;

mod audio_decoder;
mod helpers;
mod packet_reader;
mod probe;
mod stream_info;

pub use audio_decoder::AudioDecoder;
use helpers::*;
pub(crate) use packet_reader::{AudioPacketReader, DemuxedAudioPacket};
pub use probe::{ProbeOptions, probe};
use stream_info::inspect_audio;

/// The normalized PCM format emitted by this decoder: 48 kHz, stereo FL/FR,
/// 32-bit floating point, interleaved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    sample_rate: u32,
    channels: u16,
}

impl PcmFormat {
    pub const NORMALIZED: Self = Self {
        sample_rate: OUTPUT_SAMPLE_RATE_HZ,
        channels: OUTPUT_CHANNELS,
    };

    /// The normalized output sample rate, in samples per second.
    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    /// The normalized output channel count.
    pub const fn channels(self) -> u16 {
        self.channels
    }
}

/// Options controlling native input reads and cancellation.
#[derive(Clone, Debug)]
pub struct DecoderOptions {
    /// Maximum time FFmpeg may wait on an individual network read, in addition
    /// to cancellation checks. Local files are not subject to this timeout.
    pub io_timeout: Duration,
    /// A clone may be retained by another thread and cancelled while `open`
    /// or `next_frame` is blocked in FFmpeg.
    pub cancellation: CancellationToken,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        Self {
            io_timeout: Duration::from_secs(15),
            cancellation: CancellationToken::new(),
        }
    }
}

/// Metadata discovered for the selected audio stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioStreamInfo {
    pub stream_index: usize,
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    /// FFmpeg's channel-layout description, such as `stereo` or `5.1(side)`.
    /// `None` means the source did not provide a recognized layout.
    pub channel_layout: Option<String>,
    pub duration: Option<Duration>,
    pub bit_rate: Option<u64>,
    pub metadata: BTreeMap<String, String>,
}

/// One owned block of normalized PCM. Samples are interleaved as
/// `L0, R0, L1, R1, ...` and each value is clamped to `[-1.0, 1.0]`.
///
/// The PCM format is always [`PcmFormat::NORMALIZED`]. `start_sample` is the
/// frame's position on the 48 kHz output timeline. The frame length is variable
/// and is never paced or padded to a fixed duration.
#[derive(Clone, Debug, PartialEq)]
pub struct PcmFrame {
    pub samples: Vec<f32>,
    pub samples_per_channel: usize,
    pub start_sample: u64,
}

/// Internal decoder frame plus its best available source-timeline timestamp.
/// The public PCM frame continues to expose only the normalized output shape.
#[derive(Clone, Debug)]
pub(crate) struct DecodedPcmFrame {
    pub frame: PcmFrame,
    pub source_start_sample: Option<u64>,
    pub source_position_estimated: bool,
}

/// A source position associated with a processed output boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SourcePosition {
    pub sample: u64,
    pub estimated: bool,
}
