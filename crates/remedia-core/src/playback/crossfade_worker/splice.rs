//! Swapping the active source for a fallback-seek replacement at one exact sample.

use super::*;
use fallback::{FallbackSeek, Replacement, blend_head};

/// Replace the block of audio the old source just produced, `pcm`, with the replacement's audio
/// from source sample `start`, blended over the first few milliseconds.
///
/// Returns the source sample the replacement continues from. When the old source had no more
/// audio (`pcm` is empty) the replacement simply continues from where it is parked.
pub(super) fn splice(
    replacement: &mut Replacement,
    pcm: &mut Vec<f32>,
    start: u64,
    target: Duration,
) -> Result<u64, PlaybackError> {
    let count = pcm.len() / 2;
    let out_of_range = || PlaybackError::SeekOutOfRange {
        target,
        duration: Duration::ZERO,
    };
    let position = if count > 0 {
        start
    } else {
        replacement.start().ok_or_else(out_of_range)?
    };

    replacement.align(position)?;
    match replacement.start() {
        Some(parked) if parked == position => {}
        Some(_) => {
            return Err(PlaybackError::UnsupportedSeeking(
                "the replacement source's timestamps are not contiguous".to_owned(),
            ));
        }
        None => return Err(out_of_range()),
    }

    if count == 0 {
        return Ok(position);
    }

    let mut block = replacement.read_block(count)?;
    if block.len() != count * 2 {
        return Err(out_of_range());
    }

    blend_head(&mut block, Some(pcm));
    *pcm = block;

    Ok(position)
}

/// Publish the new timeline after a fallback seek took effect, as an ordinary seek does:
/// the staged next track and any scheduled preparation belonged to the old timeline.
pub(super) fn finish_fallback_switch(
    shared: &SessionShared,
    stage_slot: &Mutex<Option<Arc<PreparedStage>>>,
    preparation_requested: &mut bool,
    info: &AudioStreamInfo,
    playback_id: u64,
    done: &FallbackSeek,
    applied_sample: u64,
) {
    let stage = { lock(stage_slot).take() };

    if let Some(stage) = stage {
        shared.publish_crossfade_event(CrossfadeEvent::StageInvalidated {
            identity: stage.identity.clone(),
        });
        stage.cancel_and_join();
    }

    *preparation_requested = false;
    let (previous_generation, generation) = {
        let mut state = lock(&shared.data);
        state.duration = info.duration;
        state.info = Some(info.clone());
        state.passthrough_available = false;
        state.processing_mode = Some(PlaybackProcessingMode::Transcoding);
        let previous_generation = state.source_generation;
        state.source_serial = state.source_serial.saturating_add(1);
        state.source_generation = state.source_serial;
        state.latest_stage = None;
        shared.changed.notify_all();

        (previous_generation, state.source_generation)
    };

    shared.publish_crossfade_event(CrossfadeEvent::TimelineChanged {
        previous_generation,
        generation,
        position_ms: duration_from_samples(applied_sample).as_millis() as u64,
        requested_position_ms: done.target().as_millis() as u64,
        replaced_source: true,
    });
    fallback::record_applied(playback_id, done.target(), applied_sample, done.elapsed());
}
