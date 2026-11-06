use super::*;

pub(super) fn decoder_open_error(
    error: ffmpeg::Error,
    codec: &str,
    cancellation: &CancellationToken,
) -> DecoderError {
    if cancellation.is_cancelled() {
        return DecoderError::Cancelled;
    }
    if matches!(error, ffmpeg::Error::DecoderNotFound) {
        DecoderError::DecoderUnavailable(codec.to_owned())
    } else {
        DecoderError::DecoderInitialization(format!("{codec}: {error}"))
    }
}

pub(super) fn codec_parameter_metadata(
    parameters: &ffmpeg::codec::Parameters,
) -> (
    Option<u32>,
    Option<u16>,
    Option<u64>,
    Option<String>,
    i32,
    i32,
) {
    // SAFETY: Parameters owns a live AVCodecParameters allocation. The fields
    // read below are scalar values or a borrowed AVChannelLayout inspected
    // synchronously; no native pointer escapes this helper.
    let native = unsafe { &*parameters.as_ptr() };
    let sample_rate = u32::try_from(native.sample_rate)
        .ok()
        .filter(|sample_rate| *sample_rate > 0);
    let channels = u16::try_from(native.ch_layout.nb_channels)
        .ok()
        .filter(|channels| *channels > 0);
    let bit_rate = u64::try_from(native.bit_rate)
        .ok()
        .filter(|bit_rate| *bit_rate > 0);
    let channel_layout = unsafe { av_channel_layout_description(&native.ch_layout) };
    (
        sample_rate,
        channels,
        bit_rate,
        channel_layout,
        native.initial_padding,
        native.trailing_padding,
    )
}

pub(super) fn opus_head_passthrough_rejection(
    parameters: &ffmpeg::codec::Parameters,
    source_channels: Option<u16>,
) -> Option<&'static str> {
    // SAFETY: Parameters owns a live AVCodecParameters allocation. The
    // extradata is borrowed for the duration of this parse only.
    let native = unsafe { &*parameters.as_ptr() };
    if native.extradata.is_null() || native.extradata_size < 19 {
        return Some("Opus stream does not expose a complete OpusHead");
    }
    // SAFETY: FFmpeg guarantees extradata is readable for extradata_size bytes
    // while the owning Parameters value remains alive.
    let extra =
        unsafe { std::slice::from_raw_parts(native.extradata, native.extradata_size as usize) };
    if !extra.starts_with(b"OpusHead") || extra[8] > 1 {
        return Some("Opus codec initialization data is not a supported OpusHead");
    }
    if !matches!(extra[9], 1 | 2) || extra[18] != 0 {
        return Some("Opus channel mapping configuration is not family zero");
    }
    if source_channels != Some(u16::from(extra[9])) {
        return Some("OpusHead channel count disagrees with stream parameters");
    }
    if u16::from_le_bytes([extra[10], extra[11]]) != 0 {
        return Some("Opus stream has nonzero pre-skip");
    }
    if i16::from_le_bytes([extra[16], extra[17]]) != 0 {
        return Some("OpusHead requires output gain");
    }
    None
}

pub(super) fn is_again(error: ffmpeg::Error) -> bool {
    matches!(error, ffmpeg::Error::Other { errno } if errno == ffmpeg::error::EAGAIN)
}

pub(super) fn output_capacity(
    input_samples: usize,
    input_rate: u32,
    delay: usize,
) -> Result<usize, DecoderError> {
    let numerator = (input_samples as u128)
        .checked_mul(u128::from(OUTPUT_SAMPLE_RATE_HZ))
        .ok_or(DecoderError::FrameTooLarge)?;
    let converted = numerator
        .checked_add(u128::from(input_rate) - 1)
        .ok_or(DecoderError::FrameTooLarge)?
        / u128::from(input_rate);
    let capacity = converted
        .checked_add(delay as u128)
        .and_then(|capacity| capacity.checked_add(RESAMPLER_PADDING_SAMPLES as u128))
        .ok_or(DecoderError::FrameTooLarge)?;
    let capacity = usize::try_from(capacity).map_err(|_| DecoderError::FrameTooLarge)?;
    if capacity > i32::MAX as usize {
        return Err(DecoderError::FrameTooLarge);
    }
    Ok(capacity)
}

pub(super) fn resampler_delay_output(context: &Resampler) -> usize {
    // SAFETY: `context` owns a live SwrContext and the call only reads its
    // current delay. Decoder access is exclusive while this helper runs.
    let delay = unsafe {
        ffmpeg::ffi::swr_get_delay(context.as_ptr() as *mut _, i64::from(OUTPUT_SAMPLE_RATE_HZ))
    };
    delay.max(0) as usize
}

pub(super) fn frame_with_channel_layout(
    source: &ffmpeg::frame::Audio,
    channel_layout: ChannelLayout,
) -> Result<ffmpeg::frame::Audio, DecoderError> {
    let mut normalized = ffmpeg::frame::Audio::empty();
    // SAFETY: both frames are valid FFmpeg frames. `av_frame_ref` retains the
    // source buffers in `normalized`, so they remain alive for the conversion;
    // dropping the new frame releases only its own references.
    let result = unsafe { ffmpeg::ffi::av_frame_ref(normalized.as_mut_ptr(), source.as_ptr()) };
    if result < 0 {
        return Err(DecoderError::Resample(
            ffmpeg::Error::from(result).to_string(),
        ));
    }
    if normalized.channel_layout().is_empty() {
        normalized.set_channel_layout(channel_layout);
    }
    Ok(normalized)
}

pub(super) fn normalize_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

pub(super) fn timestamp_to_output_sample(
    timestamp: Option<i64>,
    time_base: Rational,
    sample_rate: u32,
) -> Option<u64> {
    let timestamp = timestamp?;
    if timestamp < 0 || time_base.numerator() <= 0 || time_base.denominator() <= 0 {
        return None;
    }

    let numerator = i128::from(timestamp)
        .checked_mul(i128::from(time_base.numerator()))?
        .checked_mul(i128::from(sample_rate))?;
    let samples = numerator / i128::from(time_base.denominator());
    u64::try_from(samples).ok()
}

pub(super) fn duration_from_av_time_base(duration: i64) -> Option<Duration> {
    (duration > 0).then(|| Duration::from_micros(duration as u64))
}

pub(super) fn duration_to_sample_position(position: Duration) -> Result<u64, DecoderError> {
    let samples = position.as_nanos() * u128::from(OUTPUT_SAMPLE_RATE_HZ) / 1_000_000_000;
    u64::try_from(samples).map_err(|_| {
        DecoderError::UnsupportedSeeking("the target exceeds the output timeline".to_owned())
    })
}

pub(super) trait CheckedDuration {
    fn checked_duration(self, time_base: Rational) -> Option<Duration>;
}

impl CheckedDuration for i64 {
    fn checked_duration(self, time_base: Rational) -> Option<Duration> {
        if self <= 0 || time_base.numerator() <= 0 || time_base.denominator() <= 0 {
            return None;
        }
        let seconds =
            self as f64 * f64::from(time_base.numerator()) / f64::from(time_base.denominator());
        Duration::try_from_secs_f64(seconds).ok()
    }
}

pub(super) fn frame_layout_description(frame: &ffmpeg::frame::Audio) -> Option<String> {
    // SAFETY: `frame` owns this frame and remains alive for the call.
    let frame = unsafe { frame.as_ptr() };
    if frame.is_null() {
        return None;
    }

    // SAFETY: `frame` is a live FFmpeg frame borrowed from the decoder. The
    // native `ch_layout` field is valid for the lifetime of this call.
    unsafe { av_channel_layout_description(&(*frame).ch_layout) }
}

pub(super) fn layout_description_from_mask(layout: ChannelLayout) -> Option<String> {
    if layout.is_empty() || layout.channels() <= 0 {
        return None;
    }

    // `ChannelLayout` is a legacy bitmask on FFmpeg 6 and an AVChannelLayout
    // wrapper on FFmpeg 7. Convert only through FFmpeg's public mask API; do
    // not assume the Rust wrapper has the native structure's ABI.
    let mut native_layout = std::mem::MaybeUninit::<ffmpeg::ffi::AVChannelLayout>::uninit();
    // SAFETY: FFmpeg initializes `native_layout` on success from the channel
    // mask returned by ffmpeg-next. The output is uninitialized on failure.
    let result = unsafe {
        ffmpeg::ffi::av_channel_layout_from_mask(native_layout.as_mut_ptr(), layout.bits())
    };
    if result < 0 {
        return None;
    }

    // SAFETY: `av_channel_layout_from_mask` returned success and initialized
    // the layout. It is uninitialized after `av_channel_layout_uninit` below.
    let mut native_layout = unsafe { native_layout.assume_init() };
    // SAFETY: the initialized layout remains alive and the buffer is writable
    // for its full length. FFmpeg writes a NUL-terminated description.
    let description = unsafe { av_channel_layout_description(&native_layout) };
    // SAFETY: this layout was initialized by FFmpeg and is no longer used.
    unsafe { ffmpeg::ffi::av_channel_layout_uninit(&mut native_layout) };
    description
}

pub(super) unsafe fn av_channel_layout_description(
    layout: &ffmpeg::ffi::AVChannelLayout,
) -> Option<String> {
    if layout.nb_channels <= 0
        || layout.order == ffmpeg::ffi::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC
    {
        return None;
    }

    // `char` signedness is target-dependent (notably `c_char` is unsigned on
    // some Linux targets), so use the platform C ABI alias expected by the
    // generated FFmpeg bindings.
    let mut buffer = [0 as std::ffi::c_char; 128];
    // SAFETY: the caller passes a valid AVChannelLayout and `buffer` is
    // writable for the supplied size.
    let result = unsafe {
        ffmpeg::ffi::av_channel_layout_describe(layout, buffer.as_mut_ptr(), buffer.len())
    };
    if result < 0 {
        return None;
    }
    // SAFETY: FFmpeg writes a NUL-terminated description when it succeeds.
    let description = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    Some(description.to_string_lossy().into_owned())
}
