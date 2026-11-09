//! Demuxer seeking and forward-only byte-stream trimming.

use super::*;

impl AudioDecoder {
    /// Seek to a media position. Seekable inputs use FFmpeg's demuxer, while
    /// one-shot byte streams can only move forward by decoding and discarding
    /// audio until the requested position. Backward byte-stream requests and
    /// demuxers that cannot seek return [`DecoderError::UnsupportedSeeking`].
    pub fn seek(&mut self, position: Duration) -> Result<(), DecoderError> {
        self.seek_with_pending(position, &mut None)
    }

    /// A worker may already hold one decoded frame that has not been submitted
    /// to its output. Preserve it while walking a one-shot byte stream forward.
    pub(crate) fn seek_with_pending(
        &mut self,
        position: Duration,
        pending_frame: &mut Option<DecodedPcmFrame>,
    ) -> Result<(), DecoderError> {
        let _log_capture = crate::ffmpeg_logging::capture();
        self.check_cancelled()?;
        if let Some(duration) = self.reader.info.duration
            && position > duration
        {
            return Err(DecoderError::SeekOutOfRange {
                target: position,
                duration,
            });
        }

        let timestamp = i64::try_from(position.as_micros()).map_err(|_| {
            DecoderError::UnsupportedSeeking(
                "the target exceeds FFmpeg's timestamp range".to_owned(),
            )
        })?;
        let target_sample = duration_to_sample_position(position).map_err(|_| {
            DecoderError::UnsupportedSeeking("the target exceeds the output timeline".to_owned())
        })?;
        if !self.reader.input.is_seekable() {
            return self.seek_forward(target_sample, position, pending_frame);
        }
        if let Err(error) = self.reader.input.seek(timestamp, ..) {
            if self.cancellation.is_cancelled() {
                return Err(DecoderError::Cancelled);
            }
            return Err(DecoderError::UnsupportedSeeking(error.to_string()));
        }

        if let Some(decoder) = self.decoder.as_mut() {
            decoder.flush();
        }
        self.resampler = None;
        self.pending_frames.clear();
        self.next_sample_position = None;
        self.seek_target_sample = Some(target_sample);
        self.reader.demux_finished = false;
        self.reader.pending_packets.clear();
        self.reader.next_source_sample = Some(target_sample);
        self.reader.clear_packet_history();
        self.finished = false;
        *pending_frame = None;
        Ok(())
    }

    /// Consume the source in order without retaining the skipped PCM. This is
    /// intentionally a *media* seek, not an AVIO byte offset seek: compressed
    /// media can have variable bitrates and codec-dependent timestamps.
    fn seek_forward(
        &mut self,
        target_sample: u64,
        target: Duration,
        first_frame: &mut Option<DecodedPcmFrame>,
    ) -> Result<(), DecoderError> {
        let first_available = first_frame
            .as_ref()
            .or_else(|| self.pending_frames.front())
            .map(|frame| {
                frame
                    .source_start_sample
                    .unwrap_or(frame.frame.start_sample)
            })
            .or(self.next_sample_position)
            .or_else(|| self.reader.source_boundary_sample());
        if first_available.is_some_and(|available| target_sample < available) {
            return Err(DecoderError::UnsupportedSeeking(
                "cannot seek backwards in a one-shot byte stream".to_owned(),
            ));
        }

        loop {
            self.check_cancelled()?;
            let next = match first_frame.take() {
                Some(frame) => Some(frame),
                None => self.next_timed_frame()?,
            };
            let Some(mut frame) = next else {
                let final_sample = self.next_sample_position.unwrap_or(0);
                return Err(DecoderError::SeekOutOfRange {
                    target,
                    duration: Duration::from_millis(
                        final_sample.saturating_mul(1_000) / u64::from(OUTPUT_SAMPLE_RATE_HZ),
                    ),
                });
            };

            let start = frame
                .source_start_sample
                .unwrap_or(frame.frame.start_sample);
            let end = start.saturating_add(frame.frame.samples_per_channel as u64);
            if end <= target_sample {
                continue;
            }

            let skip = target_sample.saturating_sub(start) as usize;
            if skip != 0 {
                frame
                    .frame
                    .samples
                    .drain(..skip * usize::from(OUTPUT_CHANNELS));
                frame.frame.samples_per_channel -= skip;
            }

            // FFmpeg timestamps and the decoder's running sample index may
            // differ (notably for MP3 encoder delay). Rebase the retained PCM
            // and any already decoded frames onto the source timeline.
            let retained_start = start.saturating_add(skip as u64);
            frame.frame.start_sample = retained_start;
            frame.source_start_sample = Some(retained_start);
            let mut next_sample =
                retained_start.saturating_add(frame.frame.samples_per_channel as u64);
            for buffered in &mut self.pending_frames {
                buffered.frame.start_sample = next_sample;
                next_sample = next_sample.saturating_add(buffered.frame.samples_per_channel as u64);
            }
            self.next_sample_position = Some(next_sample);

            // Put the first retained frame back for the regular playback loop.
            // Decoder state and the bounded byte-stream queue stay intact.
            self.pending_frames.push_front(frame);
            return Ok(());
        }
    }
}
