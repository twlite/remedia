use super::*;

pub(super) fn inspect_audio(input: &OpenedInput) -> Result<AudioStreamInfo, DecoderError> {
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Audio)
        .ok_or(DecoderError::NoAudioStream)?;
    let stream_index = stream.index();
    let stream_time_base = stream.time_base();
    let stream_duration = stream.duration();
    let codec_parameters = stream.parameters();
    let codec_id = codec_parameters.id();
    let stream_tags = stream
        .metadata()
        .iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect::<BTreeMap<_, _>>();
    let (sample_rate, channels, bit_rate, channel_layout, _, _) =
        codec_parameter_metadata(&codec_parameters);
    let mut metadata = input
        .metadata()
        .iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect::<BTreeMap<_, _>>();
    metadata.extend(stream_tags);
    let mut duration = stream_duration
        .checked_duration(stream_time_base)
        .or_else(|| duration_from_av_time_base(input.duration()));
    // RFC 7845: an Ogg Opus stream's final granule position counts the pre-skip, so the length of
    // the audio is shorter by that much. (Matroska carries the delay separately and its duration
    // depends on the muxer, so it is left as reported.)
    if codec_id == ffmpeg::codec::Id::OPUS
        && input.format().name().split(',').any(|name| name == "ogg")
    {
        let pre_skip = u64::from(opus_head_pre_skip(&codec_parameters));
        duration = duration.map(|duration| {
            duration.saturating_sub(Duration::from_nanos(pre_skip * 1_000_000_000 / 48_000))
        });
    }
    let info = AudioStreamInfo {
        stream_index,
        codec: codec_id.name().to_owned(),
        sample_rate,
        channels,
        channel_layout,
        duration,
        bit_rate,
        metadata,
    };
    Ok(info)
}
