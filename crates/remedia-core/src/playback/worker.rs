use super::*;

pub(super) fn worker_entry(
    shared: Arc<SessionShared>,
    generation: u64,
    source: InputSource,
    output: Arc<dyn MediaOutput>,
    cancellation: CancellationToken,
    ready: SyncSender<Result<AudioStreamInfo, PlaybackError>>,
) {
    let panic_ready = ready.clone();
    let worker_shared = Arc::clone(&shared);
    let worker_output = Arc::clone(&output);
    let worker_cancellation = cancellation.clone();
    let result = catch_unwind(AssertUnwindSafe(|| {
        worker_loop(shared, generation, source, output, cancellation, ready);
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
    loop {
        match next_worker_action(
            &shared,
            generation,
            &mut pending_frame,
            input_finished,
            can_passthrough,
            passthrough_mode,
        ) {
            WorkerAction::Exit => return,
            WorkerAction::ConfigureProcessing => {
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
                })
                .apply()
                .is_break()
                {
                    return;
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
                                    packet_info.samples_per_channel,
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
                if passthrough_mode {
                    if let Err(error) = decoder.activate_decoder(&[], None) {
                        let playback_error: PlaybackError = error.into();
                        let _ = request.response.send(Err(playback_error.clone()));
                        fail_session(&shared, generation, playback_error, &cancellation, &output);
                        return;
                    }
                    passthrough_mode = false;
                }
                // Packet passthrough cannot trim the first packet after a
                // demuxer seek to an arbitrary sample. Keep this source in the
                // decoder path after seeking so FFmpeg can discard pre-target
                // samples exactly.
                can_passthrough = false;
                {
                    let mut data = lock(&shared.data);
                    data.passthrough_available = false;
                    data.processing_mode = Some(PlaybackProcessingMode::Transcoding);
                    shared.changed.notify_all();
                }
                match decoder.seek_with_pending(request.position, &mut pending_frame) {
                    Ok(()) => {
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
                        match output.flush_samples(target_sample, &cancellation) {
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
                                let _ = request.response.send(Ok(()));
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
                        let (pending_seek, pending_filter) = {
                            let mut data = lock(&shared.data);
                            let pending = if data.generation == generation && !data.stopping {
                                data.played_position = played.or(data.played_position);
                                data.state = PlaybackState::Completed;
                                (data.pending_seek.take(), data.pending_filter_graph.take())
                            } else {
                                (None, None)
                            };
                            shared.changed.notify_all();
                            pending
                        };
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
