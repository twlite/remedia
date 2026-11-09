//! PCM conversion, resampler draining, and normalized frame timestamps.

use super::*;

impl AudioDecoder {
    pub(super) fn convert_frame(
        &mut self,
        decoded: &ffmpeg::frame::Audio,
    ) -> Result<(), DecoderError> {
        if decoded.samples() == 0 {
            return Ok(());
        }

        let input_format = decoded.format();
        let input_rate = decoded.rate();
        let input_channels = decoded.channels();
        if input_format == ffmpeg::format::Sample::None || input_rate == 0 || input_channels == 0 {
            return Err(DecoderError::InvalidAudioFormat);
        }

        let mut input_layout = decoded.channel_layout();
        if input_layout.is_empty() {
            input_layout = ChannelLayout::default(i32::from(input_channels));
        }
        if input_layout.channels() <= 0 {
            return Err(DecoderError::InvalidAudioFormat);
        }
        let input_layout_key = frame_layout_description(decoded)
            .or_else(|| layout_description_from_mask(input_layout))
            .unwrap_or_else(|| format!("{} channels", input_layout.channels()));

        if let Some(resampler) = &self.resampler {
            if resampler.input_format != input_format
                || resampler.input_layout != input_layout_key
                || resampler.input_channels != input_channels
                || resampler.input_rate != input_rate
            {
                return Err(DecoderError::FormatChanged);
            }
        } else {
            let context = Resampler::get(
                input_format,
                input_layout,
                input_rate,
                OUTPUT_SAMPLE_FORMAT,
                ChannelLayout::STEREO,
                OUTPUT_SAMPLE_RATE_HZ,
            )
            .map_err(|error| DecoderError::Resample(error.to_string()))?;
            self.resampler = Some(ResamplerState {
                context,
                input_format,
                input_layout: input_layout_key,
                input_channels,
                input_rate,
            });
        }

        let normalized_input = frame_with_channel_layout(decoded, input_layout)?;
        let resampler = self.resampler.as_mut().expect("resampler initialized");
        let delay = resampler_delay_output(&resampler.context);
        let capacity = output_capacity(decoded.samples(), input_rate, delay)?;
        let mut output =
            ffmpeg::frame::Audio::new(OUTPUT_SAMPLE_FORMAT, capacity, ChannelLayout::STEREO);
        output.set_rate(OUTPUT_SAMPLE_RATE_HZ);
        resampler
            .context
            .run(&normalized_input, &mut output)
            .map_err(|error| DecoderError::Resample(error.to_string()))?;
        self.enqueue_output(&output, decoded.timestamp())
    }

    pub(super) fn flush_resampler(&mut self) -> Result<(), DecoderError> {
        let mut output_frames = Vec::new();
        if let Some(resampler) = self.resampler.as_mut() {
            let delay = resampler_delay_output(&resampler.context);
            if delay > 0 {
                let capacity = delay
                    .checked_add(RESAMPLER_PADDING_SAMPLES)
                    .ok_or(DecoderError::FrameTooLarge)?;
                let mut output = ffmpeg::frame::Audio::new(
                    OUTPUT_SAMPLE_FORMAT,
                    capacity,
                    ChannelLayout::STEREO,
                );
                output.set_rate(OUTPUT_SAMPLE_RATE_HZ);
                resampler
                    .context
                    .flush(&mut output)
                    .map_err(|error| DecoderError::Resample(error.to_string()))?;
                output_frames.push(output);
            }
        }

        for output in output_frames {
            self.enqueue_output(&output, None)?;
        }
        Ok(())
    }

    fn enqueue_output(
        &mut self,
        output: &ffmpeg::frame::Audio,
        source_timestamp: Option<i64>,
    ) -> Result<(), DecoderError> {
        let samples_per_channel = output.samples();
        if samples_per_channel == 0 {
            return Ok(());
        }
        if output.planes() < usize::from(OUTPUT_CHANNELS) {
            return Err(DecoderError::InvalidAudioFormat);
        }

        let left = output.plane::<f32>(0);
        let right = output.plane::<f32>(1);
        if left.len() < samples_per_channel || right.len() < samples_per_channel {
            return Err(DecoderError::InvalidAudioFormat);
        }

        let source_start = timestamp_to_output_sample(
            source_timestamp,
            self.reader.stream_time_base,
            OUTPUT_SAMPLE_RATE_HZ,
        );
        let previous_next_sample = self.next_sample_position;
        let seek_target_sample = self.seek_target_sample;
        let (start_sample, trim_samples) = if let Some(target_sample) = seek_target_sample {
            let source_start = source_start.unwrap_or(target_sample);
            let source_end = source_start.saturating_add(samples_per_channel as u64);
            if source_end <= target_sample {
                return Ok(());
            }
            let trim_samples = target_sample.saturating_sub(source_start) as usize;
            let start_sample = source_start.max(target_sample);
            self.seek_target_sample = None;
            (start_sample, trim_samples)
        } else {
            (
                *self
                    .next_sample_position
                    .get_or_insert_with(|| source_start.unwrap_or(0)),
                0,
            )
        };
        let samples_per_channel = samples_per_channel.saturating_sub(trim_samples);
        if samples_per_channel == 0 {
            return Ok(());
        }
        let sample_count = samples_per_channel
            .checked_mul(usize::from(OUTPUT_CHANNELS))
            .ok_or(DecoderError::FrameTooLarge)?;
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(sample_count)
            .map_err(|_| DecoderError::FrameTooLarge)?;
        for index in trim_samples..trim_samples + samples_per_channel {
            samples.push(normalize_sample(left[index]));
            samples.push(normalize_sample(right[index]));
        }

        let (source_start_sample, source_position_estimated) =
            if let Some(target_sample) = seek_target_sample {
                if let Some(source_start) = source_start {
                    (
                        Some(source_start.saturating_add(trim_samples as u64)),
                        false,
                    )
                } else {
                    (Some(target_sample), true)
                }
            } else if let Some(source_start) = source_start {
                (Some(source_start), false)
            } else if let Some(previous_sample) = previous_next_sample {
                (Some(previous_sample), true)
            } else {
                (None, true)
            };
        self.next_sample_position = Some(start_sample.saturating_add(samples_per_channel as u64));
        self.pending_frames.push_back(DecodedPcmFrame {
            frame: PcmFrame {
                samples,
                samples_per_channel,
                start_sample,
            },
            source_start_sample,
            source_position_estimated,
        });
        Ok(())
    }
}
