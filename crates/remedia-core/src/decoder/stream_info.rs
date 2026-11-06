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
    let info = AudioStreamInfo {
        stream_index,
        codec: codec_id.name().to_owned(),
        sample_rate,
        channels,
        channel_layout,
        duration: stream_duration
            .checked_duration(stream_time_base)
            .or_else(|| duration_from_av_time_base(input.duration())),
        bit_rate,
        metadata,
    };
    Ok(info)
}
