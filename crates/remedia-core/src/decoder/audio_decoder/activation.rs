//! PCM codec activation and teardown for encoded/decoded mode transitions.

use super::*;

impl AudioDecoder {
    pub(crate) fn activate_decoder(
        &mut self,
        preroll: &[DemuxedAudioPacket],
        source_boundary: Option<u64>,
    ) -> Result<(), DecoderError> {
        self.check_cancelled()?;
        if self.decoder.is_some() {
            return Ok(());
        }
        let codec_id = self.reader.codec_parameters.id();
        let span = tracing::info_span!(
            target: "remedia_core::decoder",
            "core.decoder.initialize",
            codec = codec_id.name(),
            otel.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let result = span.in_scope(|| {
            let context = ffmpeg::codec::context::Context::from_parameters(
                self.reader.codec_parameters.clone(),
            )
            .map_err(|error| decoder_open_error(error, codec_id.name(), &self.cancellation))?;
            let mut decoder_context = context.decoder();
            if self.reader.stream_time_base.numerator() > 0
                && self.reader.stream_time_base.denominator() > 0
            {
                decoder_context.set_packet_time_base(self.reader.stream_time_base);
            }
            decoder_context
                .audio()
                .map_err(|error| decoder_open_error(error, codec_id.name(), &self.cancellation))
        });
        let decoder = match result {
            Ok(decoder) => {
                span.record("otel.status_code", "OK");
                decoder
            }
            Err(error) => {
                span.record("otel.status_code", "ERROR");
                span.record("error.type", error.kind());
                tracing::error!(
                    name: "core.decoder.initialization_failed",
                    target: "remedia_core::decoder",
                    codec = codec_id.name(),
                    error_kind = error.kind(),
                    "native audio decoder initialization failed"
                );
                return Err(error);
            }
        };
        self.decoder = Some(decoder);
        self.finished = false;
        self.seek_target_sample = source_boundary;

        if !preroll.is_empty() {
            tracing::debug!(
                name: "core.decoder.warmup_started",
                target: "remedia_core::decoder",
                codec = codec_id.name(),
                packet_count = preroll.len(),
                source_boundary_sample = source_boundary,
                "decoder warm-up replaying bounded Opus history"
            );
        }
        for packet in preroll {
            self.send_packet(&packet.packet)?;
            self.drain_decoder()?;
        }
        if !preroll.is_empty() {
            tracing::debug!(
                name: "core.decoder.warmup_completed",
                target: "remedia_core::decoder",
                codec = codec_id.name(),
                packet_count = preroll.len(),
                "decoder warm-up reached the transition boundary"
            );
        }
        Ok(())
    }

    pub(crate) fn deactivate_decoder(&mut self) -> Result<(), DecoderError> {
        if !self.pending_frames.is_empty() {
            return Err(DecoderError::Decode(
                "cannot leave PCM mode while decoded frames are pending".to_owned(),
            ));
        }
        self.decoder = None;
        self.resampler = None;
        self.next_sample_position = None;
        self.seek_target_sample = None;
        self.finished = false;
        tracing::debug!(
            name: "core.decoder.deactivated",
            target: "remedia_core::decoder",
            codec = self.reader.codec_parameters.id().name(),
            "native decoder and resampler released after passthrough transition"
        );
        Ok(())
    }

    /// Roll back an unsuccessful passthrough-to-PCM activation while retaining
    /// the demuxer and its unread input position.
    pub(crate) fn discard_decoder_activation(&mut self) {
        self.decoder = None;
        self.resampler = None;
        self.pending_frames.clear();
        self.next_sample_position = None;
        self.seek_target_sample = None;
        self.finished = false;
    }
}
