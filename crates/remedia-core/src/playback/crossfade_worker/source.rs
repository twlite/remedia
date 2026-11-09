//! Ownership and reading of direct or promoted prepared inputs.

use super::*;

/// Either the original FFmpeg decoder, or a prebuffered producer that has been
/// promoted without opening another network stream or restarting the encoder.
enum ActiveInput {
    Direct {
        decoder: Box<AudioDecoder>,
        pcm: VecDeque<f32>,
        ended: bool,
    },
    Prepared(Arc<PreparedStage>),
}

pub(super) struct ActiveSource {
    input: ActiveInput,
    pub(super) consumed: u64,
    pub(super) duration_samples: Option<u64>,
}

impl ActiveSource {
    pub(super) fn decoder(decoder: AudioDecoder) -> Self {
        let duration_samples = decoder.info().duration.map(samples_from_duration);
        Self {
            input: ActiveInput::Direct {
                decoder: Box::new(decoder),
                pcm: VecDeque::new(),
                ended: false,
            },
            consumed: 0,
            duration_samples,
        }
    }

    pub(super) fn prepared(
        stage: Arc<PreparedStage>,
        consumed: u64,
        info: &AudioStreamInfo,
    ) -> Self {
        Self {
            input: ActiveInput::Prepared(stage),
            consumed,
            duration_samples: info.duration.map(samples_from_duration),
        }
    }

    pub(super) fn read(
        &mut self,
        wanted: usize,
        cancellation: &CancellationToken,
    ) -> Result<Option<Vec<f32>>, PlaybackError> {
        if wanted == 0 {
            return Ok(None);
        }
        let samples = match &mut self.input {
            ActiveInput::Direct {
                decoder,
                pcm,
                ended,
            } => {
                while pcm.len() < wanted * 2 && !*ended && !cancellation.is_cancelled() {
                    match decoder.next_frame()? {
                        Some(frame) => pcm.extend(frame.samples),
                        None => *ended = true,
                    }
                }
                if pcm.is_empty() {
                    return Ok(None);
                }
                pcm.drain(..(wanted * 2).min(pcm.len())).collect()
            }
            ActiveInput::Prepared(stage) => {
                match stage.buffer.read_for_playback(wanted, cancellation) {
                    Some(frame) => frame,
                    None if cancellation.is_cancelled() => return Err(PlaybackError::Cancelled),
                    None if stage.snapshot().state == StageState::Failed => {
                        return Err(PlaybackError::UnsupportedSeeking(
                            "prepared source failed while decoding".into(),
                        ));
                    }
                    None => return Ok(None),
                }
            }
        };
        self.consumed += (samples.len() / 2) as u64;
        Ok(Some(samples))
    }

    pub(super) fn seek(
        &mut self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), PlaybackError> {
        match &mut self.input {
            ActiveInput::Direct {
                decoder,
                pcm,
                ended,
            } => {
                decoder.seek(position)?;
                pcm.clear();
                *ended = false;
            }
            ActiveInput::Prepared(stage) => {
                let source = stage.source.clone();
                if !source.can_seek() {
                    return Err(PlaybackError::UnsupportedSeeking(
                        "prepared byte stream cannot rewind".into(),
                    ));
                }
                let mut decoder = AudioDecoder::open_with_options(
                    source,
                    DecoderOptions {
                        cancellation: cancellation.clone(),
                        ..DecoderOptions::default()
                    },
                )?;
                decoder.seek(position)?;
                stage.cancel_and_join();
                self.input = ActiveInput::Direct {
                    decoder: Box::new(decoder),
                    pcm: VecDeque::new(),
                    ended: false,
                };
            }
        };
        self.consumed = samples_from_duration(position);
        Ok(())
    }
}

impl Drop for ActiveSource {
    fn drop(&mut self) {
        if let ActiveInput::Prepared(stage) = &self.input {
            stage.cancel_and_join();
        }
    }
}
