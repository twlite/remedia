use super::*;
use crate::decoder::{OPUS_ENCODER_LOOKAHEAD, OPUS_RECOVERY_WARM_FRAMES};

pub(super) fn worker_entry(
    shared: Arc<SessionShared>,
    generation: u64,
    source: InputSource,
    output: Arc<dyn MediaOutput>,
    cancellation: CancellationToken,
    ready: SyncSender<Result<AudioStreamInfo, PlaybackError>>,
    reopen: fallback::Reopen,
) {
    let panic_ready = ready.clone();
    let worker_shared = Arc::clone(&shared);
    let worker_output = Arc::clone(&output);
    let worker_cancellation = cancellation.clone();
    let result = catch_unwind(AssertUnwindSafe(|| {
        worker_loop(
            shared,
            generation,
            source,
            output,
            cancellation,
            ready,
            reopen,
        );
    }));

    if result.is_err() {
        let error = PlaybackError::WorkerPanicked;
        worker_cancellation.cancel();
        fail_session(
            &worker_shared,
            generation,
            error.clone(),
            &worker_cancellation,
            &worker_output,
        );
        let _ = panic_ready.try_send(Err(error));
    }
}

pub(super) fn worker_loop(
    shared: Arc<SessionShared>,
    generation: u64,
    source: InputSource,
    output: Arc<dyn MediaOutput>,
    cancellation: CancellationToken,
    ready: SyncSender<Result<AudioStreamInfo, PlaybackError>>,
    reopen: fallback::Reopen,
) {
    let _log_capture = crate::ffmpeg_logging::capture();
    let Some(startup::PreparedWorker {
        output_opus,
        mut decoder,
        mut processor,
        mut can_passthrough,
        mut passthrough_mode,
        playback_id,
        _active_playback,
    }) = startup::prepare(&shared, generation, source, &output, &cancellation, ready)
    else {
        return;
    };

    let mut pending_frame = None;
    let mut deferred_filter_update: Option<FilterUpdate> = None;
    let mut input_finished = false;
    // A seek moves the demuxer to an arbitrary sample, so the first packet must be decoded and
    // trimmed. `passthrough_eligible` records whether the stream could otherwise forward packets,
    // and `seek_recovery` tracks the return to whole source packets after that first frame.
    let mut passthrough_eligible = can_passthrough;
    let mut primed_origin: Option<u64> = None;
    let mut seek_recovery = SeekRecovery {
        pre_skip: decoder.opus_pre_skip(),
        ..SeekRecovery::default()
    };
    // A seek waiting for a second decoder to be prepared on the same track. The active source
    // keeps playing until it is ready. `force_pcm` asks for PCM output while it is, because
    // swapping decoders blends decoded audio.
    let mut fallback: Option<fallback::FallbackSeek> = None;
    let mut force_pcm = false;
    loop {
        if let Err(error) = output.wait_ready(&cancellation) {
            fail_session(&shared, generation, error.into(), &cancellation, &output);
            return;
        }
        if let Some(waiting) = fallback.as_ref() {
            waiting.set_playhead(fallback_playhead(&pending_frame, &processor));
            if waiting.is_cancelled() {
                // The request that asked for this seek gave up on it. Playback was never touched.
                fallback.take().expect("waiting seek").cancelled();
                force_pcm = false;
            } else if waiting.timed_out() {
                tracing::warn!(
                    name: "core.playback.fallback_seek_timed_out",
                    target: "remedia_core::playback",
                    playback_id,
                    waited_ms = waiting.elapsed().as_millis() as u64,
                    "fallback seek abandoned; the replacement source did not catch up"
                );
                let error = PlaybackError::UnsupportedSeeking(
                    "the replacement source did not catch up with playback in time".to_owned(),
                );
                fallback.take().expect("waiting seek").finish(Err(error));
                force_pcm = false;
            } else if waiting.is_finished() && !waiting.is_ready() {
                let outcome = waiting.take_outcome();
                let done = fallback.take().expect("waiting seek");
                force_pcm = false;
                let error = match outcome {
                    Some(Err(error)) => error,
                    _ => PlaybackError::Cancelled,
                };
                tracing::warn!(
                    name: "core.playback.fallback_seek_failed",
                    target: "remedia_core::playback",
                    playback_id,
                    error_kind = playback_error_kind(&error),
                    "fallback seek failed; the current source continues"
                );
                done.finish(Err(error));
            } else if waiting.is_ready() && passthrough_mode {
                // Swapping decoders blends decoded audio, so leave forwarded packets first.
                force_pcm = true;
            } else if waiting.is_ready() && !seek_recovery.junction {
                let done = fallback.take().expect("waiting seek");
                force_pcm = false;
                let mut replacement = match done.take_outcome() {
                    Some(Ok(replacement)) => replacement,
                    _ => {
                        done.finish(Err(PlaybackError::Cancelled));
                        continue;
                    }
                };
                // The audio the old decoder would play next, at the exact sample the swap
                // happens on. If the old source cannot produce any more, the swap still works.
                let old = match pending_frame.take() {
                    Some(frame) => Some(frame),
                    None => match decoder.next_timed_frame() {
                        Ok(frame) => frame,
                        Err(_) if is_stopped(&shared, generation) => return,
                        Err(_) => None,
                    },
                };
                // The replacement continues from the target plus the source time that has
                // passed, which is not where the old source is: the difference is the seek.
                //
                // An Opus stream that can return to packets also needs its first packet
                // boundary to coincide with a boundary of the encoder's frame, which holds
                // for exactly one phase of the replacement's start. Moving the start forward by
                // the (under 20 ms) difference makes the two grids line up, as a promoted
                // crossfade source does.
                let recover = seek_recovery.pending
                    && output.encoded_opus_output().is_some()
                    && replacement.decoder.opus_passthrough_rejection().is_none()
                    && replacement.decoder.opus_pre_skip() == OPUS_ENCODER_LOOKAHEAD;
                let skip_for = |position: u64| {
                    if recover {
                        grid_skip(
                            position,
                            processor.next_sample(),
                            seek_recovery.encoder_origin,
                        )
                    } else {
                        0
                    }
                };
                let position = old
                    .as_ref()
                    .map(|frame| {
                        let natural = done.position_for(
                            frame
                                .source_start_sample
                                .unwrap_or(frame.frame.start_sample),
                        );
                        natural + skip_for(natural)
                    })
                    .or_else(|| replacement.start().map(|start| start + skip_for(start)));
                let aligned = match position {
                    Some(position) => replacement.align(position).map(|()| position),
                    None => Err(PlaybackError::SeekOutOfRange {
                        target: done.target(),
                        duration: replacement.info.duration.unwrap_or_default(),
                    }),
                };
                let head = aligned.and_then(|position| match replacement.take_head() {
                    Some(head)
                        if head.source_start_sample.unwrap_or(head.frame.start_sample)
                            == position =>
                    {
                        Ok(head)
                    }
                    Some(_) => Err(PlaybackError::UnsupportedSeeking(
                        "the replacement source's timestamps are not contiguous".to_owned(),
                    )),
                    None => Err(PlaybackError::SeekOutOfRange {
                        target: done.target(),
                        duration: replacement.info.duration.unwrap_or_default(),
                    }),
                });
                match head {
                    // The request gave up while the replacement was being lined up.
                    Ok(_) if !done.commit() => {
                        pending_frame = old;
                        done.cancelled();
                    }
                    Ok(mut head) => {
                        fallback::blend_head(
                            &mut head.frame.samples,
                            old.as_ref().map(|frame| &frame.frame.samples[..]),
                        );
                        let applied_sample =
                            head.source_start_sample.unwrap_or(head.frame.start_sample);
                        let info = replacement.info.clone();
                        // The old decoder, and with it its connection, is released here.
                        decoder = *replacement.decoder;
                        pending_frame = Some(head);
                        input_finished = false;
                        // The replacement is decoded. If its packets can follow the re-encoded
                        // audio, the usual recovery returns to them once the encoder and the
                        // client's decoder have settled on the new audio.
                        can_passthrough = false;
                        passthrough_eligible = recover;
                        seek_recovery.pending = recover;
                        seek_recovery.pre_skip = decoder.opus_pre_skip();
                        seek_recovery.junction = false;
                        seek_recovery.delivered_frames = 0;
                        {
                            let mut data = lock(&shared.data);
                            data.duration = info.duration;
                            data.info = Some(info);
                            data.passthrough_available = false;
                            shared.changed.notify_all();
                        }
                        fallback::record_applied(
                            playback_id,
                            done.target(),
                            applied_sample,
                            done.elapsed(),
                        );
                        let outcome = done.outcome(applied_sample);
                        done.finish(Ok(outcome));
                    }
                    Err(error) => {
                        // The old source's next audio goes back where it was.
                        pending_frame = old;
                        if is_stopped(&shared, generation) {
                            return;
                        }
                        tracing::warn!(
                            name: "core.playback.fallback_seek_failed",
                            target: "remedia_core::playback",
                            playback_id,
                            error_kind = playback_error_kind(&error),
                            "fallback seek could not be applied; the current source continues"
                        );
                        done.finish(Err(error));
                    }
                }
            }
        }
        if fallback.is_none()
            && seek_recovery.ready(
                &shared,
                &decoder,
                &processor,
                &pending_frame,
                passthrough_mode,
                input_finished,
                deferred_filter_update.is_some(),
            )
        {
            // The encoder lags its input by its lookahead, so it must be given the first
            // samples of the next packet before that packet is forwarded in their place.
            seek_recovery.pending = false;
            match decoder.overshoot_next_packet(OPUS_ENCODER_LOOKAHEAD as usize) {
                Ok(Some(head)) => {
                    pending_frame = Some(head);
                    seek_recovery.junction = true;
                    tracing::debug!(
                        name: "core.playback.passthrough_recovery_junction",
                        target: "remedia_core::playback",
                        playback_id,
                        "draining the encoder lookahead before forwarding source packets again"
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    if is_stopped(&shared, generation) {
                        return;
                    }
                    fail_session(&shared, generation, error.into(), &cancellation, &output);
                    return;
                }
            }
        }
        match next_worker_action(
            &shared,
            generation,
            &mut pending_frame,
            input_finished,
            can_passthrough && !force_pcm,
            passthrough_mode,
        ) {
            WorkerAction::Exit => return,
            // The replacement finished preparing; the check at the top of the loop acts on it.
            WorkerAction::PollFallback => {}
            WorkerAction::ConfigureProcessing => {
                let was_passthrough = passthrough_mode;
                if (processing::ProcessingTransition {
                    shared: &shared,
                    generation,
                    output: &output,
                    cancellation: &cancellation,
                    playback_id,
                    decoder: &mut decoder,
                    processor: &mut processor,
                    can_passthrough,
                    passthrough_mode: &mut passthrough_mode,
                    pending_frame: &mut pending_frame,
                    deferred_filter_update: &mut deferred_filter_update,
                    seamless: seek_recovery.can_resume(),
                    primed_origin: &mut primed_origin,
                })
                .apply()
                .is_break()
                {
                    return;
                }
                if was_passthrough && !passthrough_mode && seek_recovery.can_resume() {
                    // Entering PCM from forwarded packets. A stream whose pre-skip equals the
                    // encoder lookahead returns to packets only through the lookahead junction.
                    can_passthrough = false;
                    lock(&shared.data).passthrough_available = false;
                    seek_recovery.junction = false;
                    seek_recovery.truthful = false;
                    match primed_origin.take() {
                        Some(origin) => {
                            // The encoder was primed with what the client just heard, so it is
                            // warm and its frame phase is known.
                            seek_recovery.pending = passthrough_eligible;
                            seek_recovery.delivered_frames = OPUS_RECOVERY_WARM_FRAMES;
                            seek_recovery.encoder_origin = origin;
                            seek_recovery.truthful = true;
                        }
                        // Priming was not possible (for example too little history right after
                        // a seek). Decode on: an unprimed encoder cannot be joined seamlessly.
                        None => seek_recovery.pending = false,
                    }
                }
            }
            WorkerAction::Decode => {
                if passthrough_mode {
                    match decoder.read_compressed_packet() {
                        Ok(Some(packet)) => {
                            let should_process = {
                                let data = lock(&shared.data);
                                data.filter_graph.is_some() || data.volume != 1.0
                            };
                            if should_process {
                                decoder.queue_compressed_packet(packet);
                                continue;
                            }
                            let packet_info =
                                encoded_packet_from_demuxed(&packet, decoder.info().channels);
                            let accepted = output_opus
                                .filter(|_| !packet.has_trimming_metadata)
                                .zip(packet_info.as_ref())
                                .is_some_and(|(encoded, packet)| {
                                    encoded.supports_encoded_packet(packet)
                                });
                            if accepted {
                                let Some(packet_info) = packet_info else {
                                    continue;
                                };
                                let start_sample = match processor.record_passthrough_packet(
                                    packet_info.media_samples_per_channel,
                                    packet.source_start_sample,
                                    packet.source_position_estimated,
                                ) {
                                    Ok(start_sample) => start_sample,
                                    Err(error) => {
                                        fail_session(
                                            &shared,
                                            generation,
                                            PlaybackError::FilterGraph(Arc::new(error)),
                                            &cancellation,
                                            &output,
                                        );
                                        return;
                                    }
                                };
                                let packet_info = EncodedOpusPacket {
                                    start_sample,
                                    ..packet_info
                                };
                                let submitted_end = start_sample.saturating_add(u64::from(
                                    packet_info.media_samples_per_channel,
                                ));
                                let source_end = packet.source_start_sample.map(|start| {
                                    start.saturating_add(u64::from(
                                        packet_info.media_samples_per_channel,
                                    ))
                                });
                                if let Some(encoded) = output_opus
                                    && let Err(error) =
                                        encoded.write_encoded_packet(packet_info, &cancellation)
                                {
                                    if is_stopped(&shared, generation) {
                                        return;
                                    }
                                    fail_session(
                                        &shared,
                                        generation,
                                        PlaybackError::Output(error),
                                        &cancellation,
                                        &output,
                                    );
                                    return;
                                } else {
                                    record_encoded_packet_submitted(
                                        &shared,
                                        generation,
                                        &output,
                                        submitted_end,
                                        source_end.map(|sample| SourcePosition {
                                            sample,
                                            estimated: packet.source_position_estimated,
                                        }),
                                    );
                                }
                            } else {
                                tracing::debug!(
                                    name: "core.playback.opus_packet_passthrough_rejected",
                                    target: "remedia_core::playback",
                                    playback_id,
                                    packet_bytes = packet.packet.size(),
                                    reason = if packet.has_trimming_metadata { "packet requires container trimming" } else { "packet duration or payload rejected by output" },
                                    "falling back to PCM decoding for this source"
                                );
                                decoder.queue_compressed_packet(packet);
                                can_passthrough = false;
                                lock(&shared.data).passthrough_available = false;
                                passthrough_eligible = false;
                                seek_recovery.pending = false;
                            }
                        }
                        Ok(None) => input_finished = true,
                        Err(error) => {
                            if is_stopped(&shared, generation) {
                                return;
                            }
                            fail_session(&shared, generation, error.into(), &cancellation, &output);
                            return;
                        }
                    }
                } else {
                    match decoder.next_timed_frame() {
                        Ok(Some(frame)) => pending_frame = Some(frame),
                        Ok(None) => input_finished = true,
                        Err(error) => {
                            if is_stopped(&shared, generation) {
                                return;
                            }
                            fail_session(&shared, generation, error.into(), &cancellation, &output);
                            return;
                        }
                    }
                }
            }
            WorkerAction::Deliver(decoded_frame) => {
                seek_recovery.delivered_frames += 1;
                let gain = lock(&shared.data).volume;
                let DecodedPcmFrame {
                    frame,
                    source_start_sample,
                    source_position_estimated,
                } = decoded_frame;
                match processor.write_frame(
                    frame,
                    source_start_sample,
                    source_position_estimated,
                    gain,
                    |frame, source_position| {
                        deliver_processed_frame(
                            &shared,
                            generation,
                            &output,
                            &cancellation,
                            frame,
                            source_position,
                        )
                    },
                ) {
                    Ok(()) => {
                        if std::mem::take(&mut seek_recovery.junction) {
                            // The encoder's lookahead is drained and the packet is back in the
                            // demuxer. Switch now, before anything can decode it a second time;
                            // a later gain or filter change takes the ordinary path out.
                            can_passthrough = true;
                            lock(&shared.data).passthrough_available = true;
                            if std::mem::take(&mut seek_recovery.truthful) {
                                processor.rewind(u64::from(OPUS_ENCODER_LOOKAHEAD));
                            }
                            if (processing::ProcessingTransition {
                                shared: &shared,
                                generation,
                                output: &output,
                                cancellation: &cancellation,
                                playback_id,
                                decoder: &mut decoder,
                                processor: &mut processor,
                                can_passthrough,
                                passthrough_mode: &mut passthrough_mode,
                                pending_frame: &mut pending_frame,
                                deferred_filter_update: &mut deferred_filter_update,
                                seamless: false,
                                primed_origin: &mut None,
                            })
                            .apply()
                            .is_break()
                            {
                                return;
                            }
                        }
                    }
                    Err(ProcessError::Output(_)) if is_stopped(&shared, generation) => return,
                    Err(ProcessError::Output(error)) => {
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::Output(error),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                    Err(ProcessError::Filter(error)) => {
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::FilterGraph(Arc::new(error)),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                }
            }
            WorkerAction::ReplaceFilter(update) => {
                let next_sample = processor.next_sample();
                let should_switch_mode = {
                    let data = lock(&shared.data);
                    let wants_passthrough =
                        can_passthrough && update.graph.is_none() && data.volume == 1.0;
                    let wants_transcoding = update.graph.is_some() || data.volume != 1.0;
                    (passthrough_mode && wants_transcoding)
                        || (!passthrough_mode && wants_passthrough)
                };
                let apply_while_paused = should_switch_mode
                    && !passthrough_mode
                    && lock(&shared.data).state == PlaybackState::Paused
                    && (pending_frame.is_some() || decoder.has_pending_frames());
                match output.filter_transition(next_sample, &cancellation) {
                    Ok(()) => {
                        if should_switch_mode && !apply_while_paused {
                            deferred_filter_update = Some(update);
                        } else {
                            processor.replace(update.graph);
                            let _ = update.response.send(Ok(()));
                        }
                        tracing::info!(
                            name: "core.playback.filter_graph_replaced",
                            target: "remedia_core::playback",
                            playback_id,
                            next_sample,
                            graph_enabled = lock(&shared.data).filter_graph.is_some(),
                            "PCM filtergraph replaced without resetting the output timeline"
                        );
                    }
                    Err(_) if is_stopped(&shared, generation) => {
                        let _ = update.response.send(Err(PlaybackError::Cancelled));
                        return;
                    }
                    Err(error) => {
                        let playback_error = PlaybackError::Output(error);
                        tracing::warn!(
                            name: "core.playback.filter_graph_replacement_failed",
                            target: "remedia_core::playback",
                            playback_id,
                            next_sample,
                            error_kind = "output_transition",
                            "output rejected filtergraph transition"
                        );
                        let _ = update.response.send(Err(playback_error.clone()));
                        fail_session(&shared, generation, playback_error, &cancellation, &output);
                        return;
                    }
                }
            }
            WorkerAction::Pause => match output.pause(&cancellation) {
                Ok(()) => {
                    let mut data = lock(&shared.data);
                    let paused = data.generation == generation
                        && !data.stopping
                        && data.state == PlaybackState::Pausing;
                    if paused {
                        data.state = PlaybackState::Paused;
                    }
                    shared.changed.notify_all();
                    drop(data);
                    if paused {
                        tracing::info!(
                            name: "core.playback.paused",
                            target: "remedia_core::playback",
                            playback_id,
                            "playback paused at frame boundary"
                        );
                    }
                }
                Err(_) if is_stopped(&shared, generation) => return,
                Err(error) => {
                    fail_session(
                        &shared,
                        generation,
                        PlaybackError::Output(error),
                        &cancellation,
                        &output,
                    );
                    return;
                }
            },
            WorkerAction::Resume => match output.resume(&cancellation) {
                Ok(()) => {
                    let mut data = lock(&shared.data);
                    let resumed = data.generation == generation
                        && !data.stopping
                        && data.state == PlaybackState::Resuming;
                    if resumed {
                        data.state = PlaybackState::Playing;
                    }
                    shared.changed.notify_all();
                    drop(data);
                    if resumed {
                        tracing::info!(
                            name: "core.playback.resumed",
                            target: "remedia_core::playback",
                            playback_id,
                            "playback resumed"
                        );
                    }
                }
                Err(_) if is_stopped(&shared, generation) => return,
                Err(error) => {
                    fail_session(
                        &shared,
                        generation,
                        PlaybackError::Output(error),
                        &cancellation,
                        &output,
                    );
                    return;
                }
            },
            WorkerAction::Seek(request) => {
                // Only the latest seek may replace the active source.
                if let Some(previous) = fallback.take() {
                    previous.supersede();
                    force_pcm = false;
                }
                // A decoder that cannot reposition itself is never asked to: a failed attempt
                // on an HTTP source can drop the connection and lose the current position.
                if lock(&shared.data).fallback_seeking
                    && reopen.available()
                    && !decoder.can_seek_in_place()
                {
                    if request.ticket.is_cancelled() {
                        let _ = request.response.send(Err(PlaybackError::SeekCancelled));
                        continue;
                    }
                    let origin = fallback_playhead(&pending_frame, &processor);
                    match fallback::FallbackSeek::start(
                        &shared,
                        &reopen,
                        request,
                        origin,
                        playing_info(&shared, &decoder),
                        &cancellation,
                    ) {
                        Ok(started) => {
                            tracing::info!(
                                name: "core.playback.fallback_seek_started",
                                target: "remedia_core::playback",
                                playback_id,
                                position_ms = started.target().as_millis() as u64,
                                "seek is preparing a replacement source; playback continues"
                            );
                            fallback = Some(started);
                        }
                        Err((request, error)) => {
                            let _ = request.response.send(Err(error));
                        }
                    }
                    continue;
                }
                // From here the seek changes the source, so it can no longer be cancelled.
                if !request.ticket.commit() {
                    let _ = request.response.send(Err(PlaybackError::SeekCancelled));
                    continue;
                }

                // The demuxer is asked first and nothing else is changed until it agrees, so a
                // seek that fails leaves the stream exactly as it was, passthrough included.
                match decoder.seek_with_pending(request.position, &mut pending_frame) {
                    Ok(()) => {
                        if passthrough_mode {
                            if let Err(error) = decoder.activate_decoder(&[], None) {
                                let playback_error: PlaybackError = error.into();
                                let _ = request.response.send(Err(playback_error.clone()));
                                fail_session(
                                    &shared,
                                    generation,
                                    playback_error,
                                    &cancellation,
                                    &output,
                                );
                                return;
                            }
                            passthrough_mode = false;
                        }
                        // Packet passthrough cannot trim the first packet after a
                        // demuxer seek to an arbitrary sample. Decode the packets up to
                        // the next packet boundary so FFmpeg can discard pre-target
                        // samples exactly, then `seek_recovery` restores passthrough.
                        can_passthrough = false;
                        seek_recovery.pending = passthrough_eligible && seek_recovery.can_resume();
                        seek_recovery.junction = false;
                        seek_recovery.truthful = false;
                        seek_recovery.delivered_frames = 0;
                        // The output encoder restarts on the media grid at the target.
                        seek_recovery.encoder_origin = 0;
                        {
                            let mut data = lock(&shared.data);
                            data.passthrough_available = false;
                            data.processing_mode = Some(PlaybackProcessingMode::Transcoding);
                            shared.changed.notify_all();
                        }
                        input_finished = false;
                        let target_sample = samples_from_duration(request.position);
                        if let Err(error) = processor.reset(target_sample) {
                            let playback_error = PlaybackError::FilterGraph(Arc::new(error));
                            let _ = request.response.send(Err(playback_error.clone()));
                            fail_session(
                                &shared,
                                generation,
                                playback_error,
                                &cancellation,
                                &output,
                            );
                            return;
                        }
                        let flushed =
                            output
                                .flush_samples(target_sample, &cancellation)
                                .and_then(|()| match output.encoded_opus_output() {
                                    Some(encoded) if seek_recovery.pending => {
                                        encoded.align_encoder_frames(target_sample)
                                    }
                                    _ => Ok(()),
                                });
                        match flushed {
                            Ok(()) => {
                                let played = safe_played_position(&output);
                                let mut data = lock(&shared.data);
                                if data.generation == generation && !data.stopping {
                                    data.submitted_sample = target_sample;
                                    data.submitted_position = request.position;
                                    data.media_position = Some(MediaPosition {
                                        position: request.position,
                                        accuracy: MediaPositionAccuracy::Estimated,
                                    });
                                    data.played_position = played;
                                }
                                drop(data);
                                let _ = request.response.send(Ok(SeekOutcome {
                                    requested: request.position,
                                    applied: request.position,
                                    replaced_source: false,
                                }));
                                tracing::info!(
                                    name: "core.playback.seek_completed",
                                    target: "remedia_core::playback",
                                    playback_id,
                                    position_ms = request.position.as_millis() as u64,
                                    "playback seek applied"
                                );
                            }
                            Err(_) if is_stopped(&shared, generation) => {
                                let _ = request.response.send(Err(PlaybackError::Cancelled));
                                return;
                            }
                            Err(error) => {
                                let playback_error = PlaybackError::Output(error);
                                tracing::error!(
                                    name: "core.playback.seek_failed",
                                    target: "remedia_core::playback",
                                    playback_id,
                                    error_kind = playback_error_kind(&playback_error),
                                    "playback seek failed in output"
                                );
                                let _ = request.response.send(Err(playback_error.clone()));
                                fail_session(
                                    &shared,
                                    generation,
                                    playback_error,
                                    &cancellation,
                                    &output,
                                );
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        if is_stopped(&shared, generation) {
                            let _ = request.response.send(Err(PlaybackError::Cancelled));
                            return;
                        }
                        let playback_error: PlaybackError = error.into();
                        if matches!(playback_error, PlaybackError::UnsupportedSeeking(_))
                            && lock(&shared.data).fallback_seeking
                            && reopen.available()
                        {
                            // The decoder reported that it cannot seek after all. It keeps
                            // playing from where it was while a replacement is prepared.
                            request.ticket.uncommit();
                            let origin = fallback_playhead(&pending_frame, &processor);
                            match fallback::FallbackSeek::start(
                                &shared,
                                &reopen,
                                request,
                                origin,
                                playing_info(&shared, &decoder),
                                &cancellation,
                            ) {
                                Ok(started) => fallback = Some(started),
                                Err((request, error)) => {
                                    let _ = request.response.send(Err(error));
                                }
                            }
                            continue;
                        }
                        let recoverable = matches!(
                            playback_error,
                            PlaybackError::UnsupportedSeeking(_)
                                | PlaybackError::SeekOutOfRange { .. }
                        );
                        tracing::warn!(
                            name: "core.playback.seek_failed",
                            target: "remedia_core::playback",
                            playback_id,
                            error_kind = playback_error_kind(&playback_error),
                            recoverable,
                            "playback seek was not applied"
                        );
                        let _ = request.response.send(Err(playback_error.clone()));
                        if !recoverable && !is_stopped(&shared, generation) {
                            fail_session(
                                &shared,
                                generation,
                                playback_error,
                                &cancellation,
                                &output,
                            );
                            return;
                        }
                        if is_stopped(&shared, generation) {
                            return;
                        }
                    }
                }
            }
            WorkerAction::Finish if fallback.is_some() => {
                // The old source ended while its replacement is still being prepared. Wait for
                // it, rather than complete the playback the seek was meant to continue.
                thread::sleep(Duration::from_millis(5));
            }
            WorkerAction::Finish => {
                let gain = lock(&shared.data).volume;
                match processor.finish(gain, |frame, source_position| {
                    deliver_processed_frame(
                        &shared,
                        generation,
                        &output,
                        &cancellation,
                        frame,
                        source_position,
                    )
                }) {
                    Ok(()) => {}
                    Err(ProcessError::Output(_)) if is_stopped(&shared, generation) => return,
                    Err(ProcessError::Output(error)) => {
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::Output(error),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                    Err(ProcessError::Filter(error)) => {
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::FilterGraph(Arc::new(error)),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                }
                match output.finish(&cancellation) {
                    Ok(()) => {
                        let played = safe_played_position(&output);
                        let (pending_seek, pending_filter, completed) = {
                            let mut data = lock(&shared.data);
                            let completed = data.generation == generation && !data.stopping;
                            let pending = if completed {
                                data.played_position = played.or(data.played_position);
                                data.state = PlaybackState::Completed;
                                (data.pending_seek.take(), data.pending_filter_graph.take())
                            } else {
                                (None, None)
                            };
                            shared.changed.notify_all();
                            (pending.0, pending.1, completed)
                        };
                        if completed {
                            shared.notify_completion_waiters();
                            shared.notify_crossfade_events();
                        }
                        if let Some(request) = pending_seek {
                            let _ = request.response.send(Err(PlaybackError::InvalidState {
                                operation: "seek",
                                state: PlaybackState::Completed,
                            }));
                        }
                        if let Some(update) = pending_filter {
                            // The filter setting was committed to the session before EOF won
                            // the worker race. There is no remaining PCM boundary to apply it
                            // to, so acknowledge it for the next source on this session.
                            let graph_enabled = update.graph.is_some();
                            let _ = update.response.send(Ok(()));
                            tracing::info!(
                                name: "core.playback.filter_graph_deferred",
                                target: "remedia_core::playback",
                                playback_id,
                                graph_enabled,
                                "playback completed before filter transition; configuration remains for the next source"
                            );
                        }
                        shared.observability.metrics().playback_completed();
                        tracing::info!(
                            name: "core.playback.completed",
                            target: "remedia_core::playback",
                            playback_id,
                            "playback reached end of stream"
                        );
                        return;
                    }
                    Err(_) if is_stopped(&shared, generation) => return,
                    Err(error) => {
                        fail_session(
                            &shared,
                            generation,
                            PlaybackError::Output(error),
                            &cancellation,
                            &output,
                        );
                        return;
                    }
                }
            }
        }
    }
}

mod startup;

mod processing;

/// Tracks the return to Opus packet passthrough after a seek.
///
/// The demuxer lands on a packet at or before the target, so that packet is decoded and trimmed
/// to the exact target sample. Source packets are forwarded again only once the switch is
/// seamless: the encoder has settled on the re-encoded audio, the decoded audio ends exactly on a
/// packet boundary, and the encoder lookahead can be drained with the next packet's first
/// samples. That needs the stream's pre-skip to equal the encoder's lookahead; any other stream
/// keeps decoding, as it did before.
/// Where the active source is on its own timeline: the start of the audio about to be delivered,
/// or the end of what was delivered last.
fn fallback_playhead(pending_frame: &Option<DecodedPcmFrame>, processor: &PcmProcessor) -> u64 {
    pending_frame
        .as_ref()
        .map(|frame| {
            frame
                .source_start_sample
                .unwrap_or(frame.frame.start_sample)
        })
        .or_else(|| processor.source_position().map(|position| position.sample))
        .unwrap_or(0)
}

/// The stream the session is playing, which a replacement must match.
fn playing_info(shared: &SessionShared, decoder: &AudioDecoder) -> Option<AudioStreamInfo> {
    Some(
        lock(&shared.data)
            .info
            .clone()
            .unwrap_or_else(|| decoder.info().clone()),
    )
}

/// How far to move a replacement's start forward so that the first source packet boundary after
/// it falls on a boundary of the Opus encoder's 20 ms frame.
///
/// The encoder's partial frame holds `(output_sample - encoder_origin) % 960` samples when the
/// replacement's audio begins, and packets begin where `(source sample + pre-skip) % 960 == 0`.
/// Audio fed from both sides advances them equally, so they coincide at some point exactly when
/// the start has the same phase as the encoder's.
fn grid_skip(start: u64, output_sample: u64, encoder_origin: u64) -> u64 {
    let wanted = output_sample.saturating_sub(encoder_origin) % 960;

    (wanted + 960 - start % 960) % 960
}

#[derive(Default)]
struct SeekRecovery {
    pending: bool,
    delivered_frames: u32,
    /// The first samples of the next packet are in `pending_frame` to drain the encoder.
    junction: bool,
    /// The stream's pre-skip: source packets begin where `(media sample + pre_skip) % 960 == 0`.
    pre_skip: u32,
    /// The output sample at which the encoder's frame grid starts. Its partial frame holds
    /// `(next_sample - origin) % 960` samples.
    encoder_origin: u64,
    /// Packets were accounted in heard media (the encoder was primed on forwarded audio), so the
    /// lookahead samples drained at the junction are taken back off the output timeline.
    truthful: bool,
}

impl SeekRecovery {
    fn can_resume(&self) -> bool {
        self.pre_skip == OPUS_ENCODER_LOOKAHEAD
    }

    #[allow(clippy::too_many_arguments)]
    fn ready(
        &self,
        shared: &SessionShared,
        decoder: &AudioDecoder,
        processor: &PcmProcessor,
        pending_frame: &Option<DecodedPcmFrame>,
        passthrough_mode: bool,
        input_finished: bool,
        deferred_filter: bool,
    ) -> bool {
        if !self.pending
            || !self.can_resume()
            || self.delivered_frames < OPUS_RECOVERY_WARM_FRAMES
            || passthrough_mode
            || input_finished
            || deferred_filter
            || pending_frame.is_some()
            || decoder.has_pending_frames()
            || !(processor.next_sample().saturating_sub(self.encoder_origin)
                + u64::from(self.pre_skip))
            .is_multiple_of(960)
        {
            return false;
        }

        // The delivered PCM must end exactly at the demuxer's next packet. Estimated positions
        // (missing timestamps or an active filtergraph) cannot prove that.
        let exact_boundary = processor.source_position().is_some_and(|position| {
            !position.estimated && Some(position.sample) == decoder.source_boundary_sample()
        });
        if !exact_boundary {
            return false;
        }

        let data = lock(&shared.data);
        data.filter_graph.is_none() && data.volume == 1.0 && data.state == PlaybackState::Playing
    }
}

#[cfg(test)]
mod grid_tests {
    use super::grid_skip;

    #[test]
    fn a_start_on_the_encoders_phase_is_not_moved() {
        // 1_000 samples into a frame grid that began at 0: phase 40.
        assert_eq!(grid_skip(40, 1_000, 0), 0);
        assert_eq!(grid_skip(40 + 960 * 7, 1_000, 0), 0);
    }

    #[test]
    fn a_start_is_moved_forward_to_the_encoders_phase_by_less_than_a_frame() {
        for start in [0, 1, 39, 41, 500, 959, 12_345] {
            for origin in [0, 123, 960] {
                let skip = grid_skip(start, 50_000, origin);
                assert!(skip < 960);
                assert_eq!(
                    (start + skip) % 960,
                    (50_000_u64 - origin) % 960,
                    "start {start}, origin {origin}"
                );
            }
        }
    }
}
