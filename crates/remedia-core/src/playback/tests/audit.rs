//! Receiver-side audit of Opus passthrough: what a client that decodes the RTP payloads hears.
//!
//! The "client" is a fresh libopus decoder (RTP Opus has no pre-skip), the "reference" is FFmpeg's
//! decode of the same file with pre-skip applied. Metrics are about time alignment and
//! discontinuities, not only energy residuals.

use super::preskip::*;
use super::*;

/// Mono mix of an interleaved stereo slice.
fn mono(samples: &[f32]) -> Vec<f32> {
    samples
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| (pair[0] + pair[1]) * 0.5)
        .collect()
}

/// Best alignment of `len` client samples starting at client index `client` against the
/// reference starting at media sample `media`, searching +-`reach` samples. Returns the lag
/// (client minus nominal, in samples) and the residual relative to the signal in dB.
pub(super) fn best_alignment(
    client: &[f32],
    reference: &[f32],
    client_start: usize,
    media_start: usize,
    len: usize,
    reach: i32,
    gain: f32,
) -> (i32, f32) {
    let client = mono(client);
    let reference = mono(reference);
    let signal: f32 = reference[media_start..media_start + len]
        .iter()
        .map(|v| (v * gain).powi(2))
        .sum();
    let mut best = (0, f32::MAX);
    for lag in -reach..=reach {
        let begin = client_start as i32 + lag;
        if begin < 0 || begin as usize + len > client.len() {
            continue;
        }
        let begin = begin as usize;
        let error: f32 = (0..len)
            .map(|n| (client[begin + n] - reference[media_start + n] * gain).powi(2))
            .sum();
        if error < best.1 {
            best = (lag, error);
        }
    }
    (best.0, 10.0 * (best.1 / signal.max(1e-12)).log10())
}

/// Everything the sink receives when `bytes` plays to the end through a sink that cannot take
/// packets, so every packet is re-encoded from PCM.
fn transcode_to_end(bytes: &[u8]) -> Vec<EncodedOpusPacket> {
    let (sink, handle) = StepPacketSink::without_passthrough();
    handle.release(100_000);
    let session = PlaybackSession::new();
    session
        .play(
            InputSource::bytes(bytes.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert_eq!(session.wait(), PlaybackState::Completed);
    handle.snapshot().packets
}

#[test]
fn passthrough_and_transcoded_output_are_time_aligned_at_the_receiver() {
    // RTP Opus carries no pre-skip, so a client plays the encoder delay at the start of any
    // stream. What matters is that forwarding does not make that delay different from the one
    // the transcoding path has always had: both place media sample n at client sample n + 312.
    for (name, bytes) in [
        ("abrupt music", ABRUPT),
        ("silent lead", SILENT_LEAD_LONG),
        ("transients", TRANSIENTS),
    ] {
        let reference = reference_pcm(bytes);
        let forwarded = play_to_end(bytes);
        let transcoded = transcode_to_end(bytes);
        let source = source_packets(bytes);
        assert_eq!(
            payload(&source[0]),
            forwarded[0].bytes,
            "{name}: the stream is forwarded, not silently transcoded"
        );
        let (forwarded_pcm, transcoded_pcm) = (client_pcm(&forwarded), client_pcm(&transcoded));
        let reach = reference.len() / 2 - 5_000;
        let mut checked = 0;
        for at in [PRE_SKIP * 4, 20_000, reach / 3, reach / 2, reach * 2 / 3] {
            // A silent window has no alignment to find.
            let energy: f32 = reference[at * 2..(at + 4_800) * 2]
                .iter()
                .map(|v| v * v)
                .sum();
            if energy < 1.0 {
                continue;
            }
            checked += 1;
            let (lag_forwarded, _) = best_alignment(
                &forwarded_pcm,
                &reference,
                at + PRE_SKIP,
                at,
                4_800,
                700,
                1.0,
            );
            let (lag_transcoded, _) = best_alignment(
                &transcoded_pcm,
                &reference,
                at + PRE_SKIP,
                at,
                4_800,
                700,
                1.0,
            );
            assert_eq!(
                lag_forwarded, 0,
                "{name} at {at}: forwarded audio is 312 samples late"
            );
            assert_eq!(
                lag_transcoded, 0,
                "{name} at {at}: transcoded audio is 312 samples late"
            );
        }
        assert!(checked >= 3, "{name}: enough audible windows were compared");
        // The pre-roll a client plays before the audio is encoder delay: silence, or pre-echo
        // well below the onset it precedes. Forwarding can leave more pre-echo than the
        // transcoder's own encoder would (it re-encodes already trimmed audio), so bound it
        // against the onset rather than against the transcoder.
        let peak = |pcm: &[f32]| {
            pcm[..PRE_SKIP * 2]
                .iter()
                .fold(0.0_f32, |m, v| m.max(v.abs()))
        };
        let onset = |pcm: &[f32]| {
            pcm[PRE_SKIP * 2..(PRE_SKIP + 960) * 2]
                .iter()
                .fold(0.0_f32, |m, v| m.max(v.abs()))
        };
        assert!(
            peak(&forwarded_pcm) <= 1e-3_f32.max(0.35 * onset(&forwarded_pcm)),
            "{name}: pre-roll {} against onset {}",
            peak(&forwarded_pcm),
            onset(&forwarded_pcm)
        );
        println!(
            "{name}: pre-roll peak forwarded {:.5}, transcoded {:.5}, onset {:.3}",
            peak(&forwarded_pcm),
            peak(&transcoded_pcm),
            onset(&forwarded_pcm)
        );
    }
}

#[test]
fn a_mislabeled_pre_skip_is_decoded_not_forwarded() {
    // The OpusHead claims 600 samples of pre-skip over audio that is already at full level, so
    // forwarding the first packet would play real audio the container asks a player to drop.
    let bytes = include_bytes!("../../../tests/fixtures/ffmpeg-opus-mislabeled-preskip.ogg");
    let source = source_packets(bytes);
    assert!(!source.is_empty());
    let (sink, handle) = StepPacketSink::new();
    handle.release(100_000);
    let session = PlaybackSession::new();
    session
        .play(
            InputSource::bytes(bytes.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Transcoding)
    );
    let packets = handle.snapshot().packets;
    assert!(
        !packets
            .iter()
            .any(|p| source.iter().any(|s| payload(s) == p.bytes)),
        "nothing is forwarded"
    );
}

/// Play `bytes`, applying each `(packets_sent_before, gain)` change while the worker is blocked
/// on the sink, and return every packet the sink received.
fn play_with_gain_changes(bytes: &[u8], changes: &[(usize, f32)]) -> Vec<EncodedOpusPacket> {
    play_with_gain_changes_in(bytes, changes, false)
}

/// As [`play_with_gain_changes`], optionally on the crossfade worker (no next source staged).
fn play_with_gain_changes_in(
    bytes: &[u8],
    changes: &[(usize, f32)],
    crossfade: bool,
) -> Vec<EncodedOpusPacket> {
    let (sink, handle) = StepPacketSink::new();
    let session = Arc::new(PlaybackSession::new());
    if crossfade {
        session
            .configure_crossfade(Some(
                super::super::CrossfadeConfig::new(1000, 5000).unwrap(),
            ))
            .unwrap();
    }
    session
        .play(
            InputSource::bytes(bytes.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert!(handle.wait_for_calls(1));
    let mut sent = 0;
    for &(before, gain) in changes {
        handle.release(before - sent);
        assert!(handle.wait_for_calls(before + 1));
        let changing = Arc::clone(&session);
        let update = thread::spawn(move || changing.set_volume(gain));
        let deadline = Instant::now() + TEST_TIMEOUT;
        while session.volume() != gain {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        handle.release(1);
        assert!(
            handle.wait_for_calls(before + 2),
            "worker stalled after a gain change at packet {before}: {:?} {:?}",
            session.state(),
            session.last_error()
        );
        update.join().unwrap().unwrap();
        sent = before + 1;
    }
    handle.release(100_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    handle.snapshot().packets
}

/// Worst absolute sample-to-sample step of `samples` (interleaved stereo) in `[from, to)`.
fn max_step(samples: &[f32], from: usize, to: usize) -> f32 {
    (from + 1..to)
        .flat_map(|n| (0..2).map(move |c| (samples[n * 2 + c] - samples[(n - 1) * 2 + c]).abs()))
        .fold(0.0, f32::max)
}

/// Per-block `(lag, residual dB)` of the client against the reference, where media block
/// `first + k` is expected at client sample `media + 312` and carries `gain(media)`.
fn blocks(
    client: &[f32],
    reference: &[f32],
    first_block: usize,
    count: usize,
    gain: impl Fn(usize) -> f32,
) -> Vec<(i32, f32)> {
    (first_block..first_block + count)
        .map(|block| {
            let media = block * 960;
            let rms = |samples: &[f32]| {
                (samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32).sqrt()
            };
            let wanted = &reference[media * 2..(media + 960) * 2];
            if rms(wanted) * gain(media).max(0.01) < 0.004 {
                // A near-silent block has no meaningful relative error or alignment. It must
                // simply stay near-silent: nothing audible was inserted.
                let heard = &client[(media + PRE_SKIP) * 2..(media + PRE_SKIP + 960) * 2];
                return (0, if rms(heard) < 0.01 { -100.0 } else { 0.0 });
            }
            best_alignment(
                client,
                reference,
                media + PRE_SKIP,
                media,
                960,
                64,
                gain(media),
            )
        })
        .collect()
}

/// Assert the client's audio around `event_media` is continuous in time (no inserted or dropped
/// samples) and has no block that is far from the reference (a click or dropout).
fn assert_continuous(
    label: &str,
    report: &[(i32, f32)],
    skip: std::ops::Range<usize>,
    limit_db: f32,
) {
    for (index, (lag, db)) in report.iter().enumerate() {
        if skip.contains(&index) {
            continue;
        }
        assert!(
            lag.abs() <= 1,
            "{label}: block {index} is {lag} samples out of time ({report:?})"
        );
        assert!(
            *db < limit_db,
            "{label}: block {index} residual {db:.1} dB ({report:?})"
        );
    }
}

#[test]
fn a_mid_song_gain_change_neither_inserts_nor_drops_samples_in_either_direction() {
    for (name, bytes, limit) in [
        ("silent lead", SILENT_LEAD_LONG, -9.0),
        ("transients", TRANSIENTS, -9.0),
    ] {
        let reference = reference_pcm(bytes);
        // 0.5 from packet 120 to packet 240 and then 1.0 again.
        let packets = play_with_gain_changes(bytes, &[(120, 0.5), (240, 1.0)]);
        let client = client_pcm(&packets);
        assert_timeline_is_contiguous(&packets);

        let to = 121 * 960 - PRE_SKIP;
        let back = 242 * 960 - PRE_SKIP;
        let gain = move |media: usize| {
            if (to..back).contains(&media) {
                0.5
            } else {
                1.0
            }
        };
        let report = blocks(&client, &reference, 100, 160, gain);
        // The two blocks around each gain step compare against the wrong gain.
        let around = |media: usize| (media / 960).saturating_sub(100);
        let skip_to = around(to).saturating_sub(1)..around(to) + 3;
        let skip_back = around(back).saturating_sub(1)..around(back) + 6;
        let report_no_steps: Vec<_> = report
            .iter()
            .enumerate()
            .filter(|(i, _)| !skip_to.contains(i) && !skip_back.contains(i))
            .map(|(_, r)| *r)
            .collect();
        assert_continuous(name, &report_no_steps, 0..0, limit);

        // Sample continuity right at the seams: no step larger than the signal itself has.
        for (event, label) in [(to, "to PCM"), (back, "back to packets")] {
            let window = event..event + 4_800;
            let heard = max_step(&client, window.start + PRE_SKIP, window.end + PRE_SKIP);
            let expected = max_step(&reference, window.start, window.end) * 0.5_f32.max(1.0);
            assert!(
                heard <= expected * 1.5 + 0.05,
                "{name} {label}: step {heard} against {expected} in the reference"
            );
        }
        // No audio is lost or duplicated overall.
        assert_eq!(
            media_samples(&packets) as usize,
            reference.len() / 2,
            "{name}: media length"
        );
    }
}

/// Gain-agnostic alignment: the lag in `-reach..=reach` that maximizes the normalized correlation
/// between a client block and the reference block. `None` for a near-silent block.
fn correlated_lag(
    client: &[f32],
    reference: &[f32],
    media: usize,
    len: usize,
    reach: i32,
) -> Option<i32> {
    let client = mono(client);
    let reference = mono(reference);
    let wanted = &reference[media..media + len];
    let energy: f32 = wanted.iter().map(|v| v * v).sum();
    if (energy / len as f32).sqrt() < 0.004 {
        return None;
    }
    // Periodic content (a tone burst) correlates equally well at several lags, so a lag counts
    // only when it is clearly better than the nominal one.
    let correlation = |lag: i32| -> Option<f32> {
        let begin = (media + PRE_SKIP) as i32 + lag;
        if begin < 0 || begin as usize + len > client.len() {
            return None;
        }
        let heard = &client[begin as usize..begin as usize + len];
        let dot: f32 = heard.iter().zip(wanted).map(|(a, b)| a * b).sum();
        let norm = heard.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-9);
        Some(dot / norm)
    };
    let nominal = correlation(0)?;
    let mut best = (0, nominal);
    for lag in -reach..=reach {
        if let Some(value) = correlation(lag)
            && value > best.1
        {
            best = (lag, value);
        }
    }
    Some(if best.1 > nominal + 0.05 * nominal.abs() {
        best.0
    } else {
        0
    })
}

fn assert_all_blocks_in_time(
    label: &str,
    client: &[f32],
    reference: &[f32],
    first: usize,
    last: usize,
) {
    for block in first..last.min(reference.len() / 2 / 960 - 2) {
        if let Some(lag) = correlated_lag(client, reference, block * 960, 960, 64) {
            assert!(
                lag.abs() <= 1,
                "{label}: block {block} is {lag} samples out of time"
            );
        }
    }
}

#[test]
fn gain_changes_on_the_crossfade_worker_are_seamless_too() {
    for (name, bytes) in [
        ("silent lead", SILENT_LEAD_LONG),
        ("transients", TRANSIENTS),
    ] {
        let reference = reference_pcm(bytes);
        let packets = play_with_gain_changes_in(bytes, &[(120, 0.5), (240, 1.0)], true);
        let client = client_pcm(&packets);
        assert_timeline_is_contiguous(&packets);
        assert_all_blocks_in_time(name, &client, &reference, 100, 300);
        let source = source_packets(bytes);
        let (_, _, run) = longest_forwarded_run(&packets, &source, 200);
        assert!(
            run > 15,
            "{name}: packets resume after the gain returns to 1.0 ({run})"
        );
    }
}

#[test]
fn a_gain_change_at_every_point_around_the_return_to_packets_never_repeats_or_drops_audio() {
    // The return to packets drains the encoder with the next packet's first samples and then puts
    // that packet back. Changing the gain again at each packet around that moment exercises the
    // path where the plan is abandoned half way.
    let reference = reference_pcm(TRANSIENTS);
    for offset in [0, 1, 2, 3, 4, 6, 9] {
        for crossfade in [false, true] {
            let at = 140 + offset;
            let packets = play_with_gain_changes_in(
                TRANSIENTS,
                &[(100, 0.5), (at, 1.0), (at + 1, 0.5), (at + 1 + 10, 1.0)],
                crossfade,
            );
            let client = client_pcm(&packets);
            assert_all_blocks_in_time(
                &format!("gain flip at {at} (crossfade {crossfade})"),
                &client,
                &reference,
                95,
                185,
            );
        }
    }
}

#[test]
fn seeking_into_transients_and_quiet_passages_resumes_packets_without_a_click() {
    // 510 ms is inside a 12 ms tone burst, 3 200 ms inside the near-silent gap, 4 003 ms inside the
    // burst right after it, and 1 250 ms inside the drone between bursts.
    for millis in [510_u64, 1_250, 3_200, 4_003] {
        assert_seek_is_seamless_in(TRANSIENTS, millis * 48, [-8.0, -18.0, -6.0]);
    }
}

#[test]
fn repeated_seeks_in_a_row_each_end_with_a_clean_seam() {
    // Each seek lands before the previous one has returned to packets.
    let reference = reference_pcm(TRANSIENTS);
    let source = source_packets(TRANSIENTS);
    let (sink, handle) = StepPacketSink::new();
    let session = Arc::new(PlaybackSession::new());
    session
        .play(
            InputSource::bytes(TRANSIENTS.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert!(handle.wait_for_calls(1));
    let mut calls = 1;
    let targets = [2_000_u64, 500, 5_005, 1_333, 6_100];
    for millis in targets {
        let seeking = Arc::clone(&session);
        let (tx, rx) = mpsc::sync_channel(1);
        let seeker = thread::spawn(move || {
            let _ = tx.send(seeking.seek(Duration::from_millis(millis)));
        });
        assert!(handle.wait_for_pending_seek(&session));
        handle.release(1);
        rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
        seeker.join().unwrap();
        // Let a few packets through so each seek lands mid-recovery, then block again.
        calls += 1;
        assert!(handle.wait_for_calls(calls));
        handle.release(3);
        calls += 3;
        assert!(handle.wait_for_calls(calls));
    }
    handle.release(100_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    let packets = handle.snapshot().packets;
    let last_seek = *targets.last().unwrap() * 48;
    // Everything after the last seek: the first packet starts at the target, the output is
    // contiguous, and packets resume.
    let start = packets
        .iter()
        .rposition(|p| p.start_sample == last_seek)
        .unwrap();
    let after = &packets[start..];
    assert!(after.windows(2).all(|pair| pair[1].start_sample
        == pair[0].start_sample + u64::from(pair[0].media_samples_per_channel)));
    let (_, _, run) = longest_forwarded_run(after, &source, 0);
    assert!(run > 30, "packets resume after the last seek ({run})");
    // And its audio lines up with the reference across the seam.
    let (junction, k) = forwarded_run(after, &source);
    let client = client_pcm(after);
    let junction_client: usize = after[..junction]
        .iter()
        .map(|p| p.samples_per_channel as usize)
        .sum();
    let report = residual_db(&client, junction_client, &reference, 960 * k - PRE_SKIP, 8);
    assert!(
        report[0] < -8.0 && report[7] < -18.0,
        "seam after repeated seeks: {report:?}"
    );
}

#[test]
fn consecutive_promotions_of_real_files_each_resume_packets_with_a_clean_seam() {
    // silent lead (1.8 s) -> transients (8 s) -> silent lead long (6 s): two overlaps and two
    // decoder handoffs on one encoder.
    let transients = reference_pcm(TRANSIENTS);
    let long = reference_pcm(SILENT_LEAD_LONG);
    let transients_source = source_packets(TRANSIENTS);
    let long_source = source_packets(SILENT_LEAD_LONG);

    let (session, handle) = start_crossfade(SILENT_LEAD, TRANSIENTS);
    // Play the first overlap and part of the second track, then stage the third.
    handle.release(170);
    assert!(handle.wait_for_calls(171));
    assert_eq!(
        session.source_generation(),
        2,
        "the first promotion happened"
    );
    session
        .stage_next(
            InputSource::bytes(SILENT_LEAD_LONG.to_vec()),
            super::super::StageIdentity {
                expected_generation: session.source_generation(),
                entry_id: "third".into(),
                queue_revision: 2,
                source_revision: 1,
            },
        )
        .unwrap();
    let deadline = Instant::now() + TEST_TIMEOUT;
    while session.staged_source().unwrap().buffered_samples < 96_000 {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    handle.release(100_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(
        session.source_generation(),
        3,
        "the second promotion happened"
    );

    let packets = handle.snapshot().packets;
    assert_timeline_is_contiguous(&packets);
    let client = client_pcm(&packets);

    // Second track: forwarded after the first overlap, with a clean seam.
    let (first, k1, run1) = longest_forwarded_run(&packets, &transients_source, 100);
    assert!(run1 > 60, "the second track resumes packets ({run1})");
    let at = packets[..first]
        .iter()
        .map(|p| p.samples_per_channel as usize)
        .sum();
    let seam1 = residual_db(&client, at, &transients, 960 * k1 - PRE_SKIP, 8);
    assert!(seam1[0] < -8.0 && seam1[7] < -18.0, "first seam: {seam1:?}");

    // Third track: forwarded after the second overlap.
    let (second, k2, run2) = longest_forwarded_run(&packets, &long_source, 100);
    assert!(run2 > 15, "the third track resumes packets ({run2})");
    let at = packets[..second]
        .iter()
        .map(|p| p.samples_per_channel as usize)
        .sum();
    let seam2 = residual_db(&client, at, &long, 960 * k2 - PRE_SKIP, 8);
    assert!(
        seam2[0] < -15.0 && seam2[7] < -30.0,
        "second seam: {seam2:?}"
    );
}

/// Media position, in samples, that a listener has heard through the end of each packet, given
/// that the client plays encoded duration continuously and that media sample `m` is played at
/// client sample `m + 312` when the encoder started at media sample `origin`.
fn heard_ends(packets: &[EncodedOpusPacket], origin: u64) -> Vec<u64> {
    let mut played = 0_u64;
    packets
        .iter()
        .map(|packet| {
            played += u64::from(packet.samples_per_channel);
            (origin + played).saturating_sub(PRE_SKIP as u64)
        })
        .collect()
}

#[test]
fn durations_of_real_ogg_files_match_the_audio_they_decode_to() {
    for (name, bytes) in [
        ("silent lead", SILENT_LEAD),
        ("abrupt", ABRUPT),
        ("transients", TRANSIENTS),
        ("long", SILENT_LEAD_LONG),
    ] {
        let decoded = (reference_pcm(bytes).len() / 2) as u64;
        let reader = AudioDecoder::open_for_playback(
            InputSource::bytes(bytes.to_vec()),
            DecoderOptions::default(),
        )
        .unwrap();
        let reported = reader
            .info()
            .duration
            .map(super::super::samples_from_duration)
            .unwrap();
        assert!(
            reported.abs_diff(decoded) <= 1,
            "{name}: reported {reported}, decoded {decoded}"
        );
    }
}

#[test]
fn reported_positions_equal_what_the_listener_has_heard_through_gain_changes() {
    for crossfade in [false, true] {
        let packets = play_with_gain_changes_in(TRANSIENTS, &[(120, 0.5), (240, 1.0)], crossfade);
        let heard = heard_ends(&packets, 0);
        for (index, (packet, heard)) in packets
            .iter()
            .zip(&heard)
            .enumerate()
            .take(packets.len() - 2)
        {
            assert_eq!(
                packet.start_sample + u64::from(packet.media_samples_per_channel),
                *heard,
                "packet {index} (crossfade {crossfade}): reported end against heard end"
            );
        }
    }
}

#[test]
fn after_a_seek_reported_positions_lead_the_heard_audio_by_the_encoder_lookahead_only() {
    // A seek restarts the encoder, whose packets are accounted by their input position; that is
    // the convention PCM output has always had. The lead is one lookahead, and constant.
    for millis in [2_000_u64, 2_510] {
        let target = millis * 48;
        let packets = packets_after_seek(TRANSIENTS, target);
        let origin = target - target % 960;
        let heard = heard_ends(&packets, origin);
        for (index, (packet, heard)) in packets
            .iter()
            .zip(&heard)
            .enumerate()
            .take(packets.len() - 2)
        {
            let reported = packet.start_sample + u64::from(packet.media_samples_per_channel);
            assert_eq!(
                reported,
                heard + PRE_SKIP as u64,
                "seek to {millis} ms, packet {index}"
            );
        }
    }
}

#[test]
fn the_media_position_the_api_reports_is_exact_through_a_seek() {
    let (sink, handle) = StepPacketSink::new();
    let session = Arc::new(PlaybackSession::new());
    session
        .play(
            InputSource::bytes(TRANSIENTS.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert!(handle.wait_for_calls(1));
    handle.release(30);
    assert!(handle.wait_for_calls(31));
    // 30 packets of the straight, forwarded start: exactly 30 * 20 ms - 6.5 ms of media.
    assert_eq!(
        session.position().media.unwrap().position,
        Duration::from_nanos((30 * 960 - PRE_SKIP as u64) * 1_000_000_000 / 48_000)
    );

    let seeking = Arc::clone(&session);
    let seeker = thread::spawn(move || seeking.seek(Duration::from_millis(5_000)));
    assert!(handle.wait_for_pending_seek(&session));
    handle.release(1);
    seeker.join().unwrap().unwrap();
    // The position jumps to the target. The worker may already have accepted the first, partial
    // re-encoded frame, so allow it to be a frame or two past the target.
    let after = session.position().media.unwrap();
    assert!(
        (Duration::from_millis(5_000)..Duration::from_millis(5_040)).contains(&after.position),
        "the position jumps to the target: {:?}",
        after.position
    );
    // Right after the seek the position is an estimate; once frames are delivered it is a
    // timestamp. Either is acceptable here.

    // Afterwards the position only moves forward and ends at the track's end.
    let mut last = after.position;
    for _ in 0..40 {
        handle.release(1);
        std::thread::sleep(Duration::from_millis(1));
        if let Some(media) = session.position().media {
            assert!(
                media.position >= last,
                "media position went backwards: {last:?} -> {:?}",
                media.position
            );
            last = media.position;
        }
    }
    handle.release(100_000);
    session.wait();
    let end = session.position().media.unwrap().position;
    assert!(
        end.abs_diff(Duration::from_secs(8)) < Duration::from_millis(25),
        "ended at {end:?}"
    );
}

#[test]
fn duration_and_media_position_follow_a_crossfade_promotion() {
    let (session, handle) = start_crossfade(SILENT_LEAD, TRANSIENTS);
    assert_eq!(session.duration(), Some(Duration::from_millis(1_800)));
    handle.release(100_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(session.source_generation(), 2);
    assert_eq!(
        session.duration(),
        Some(Duration::from_secs(8)),
        "the incoming track's length"
    );
    let media = session.position().media.unwrap();
    assert!(
        media.position.abs_diff(Duration::from_secs(8)) < Duration::from_millis(25),
        "the position ended at {:?}",
        media.position
    );
}

#[test]
fn media_position_tracks_source_time_through_a_tempo_change_and_back() {
    // atempo=1.5 consumes 30 ms of source per 20 ms packet. The reported media position is source
    // time, so it advances 1.5x as fast while the filter is on and normally after it is removed.
    let (sink, handle) = StepPacketSink::new();
    let session = Arc::new(PlaybackSession::new());
    session
        .play(
            InputSource::bytes(TRANSIENTS.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    // The worker is always blocked on packet `sent + 1`.
    let step = |count: usize, sent: &mut usize| {
        handle.release(count);
        *sent += count;
        assert!(handle.wait_for_calls(*sent + 1));
    };
    let mut sent = 0;
    assert!(handle.wait_for_calls(1));
    step(50, &mut sent);
    let position = |session: &PlaybackSession| session.position().media.unwrap().position;
    let at_start_of_tempo = position(&session);

    let tempo = Arc::clone(&session);
    let apply = thread::spawn(move || tempo.set_filter_graph(Some("atempo=1.5")));
    assert!(handle.wait_for_pending_filter(&session));
    step(1, &mut sent);
    apply.join().unwrap().unwrap();
    step(100, &mut sent);
    let after_tempo = position(&session);
    let advanced = after_tempo - at_start_of_tempo;
    // 101 packets of 20 ms output at 1.5x, give or take the filter's own buffering.
    assert!(
        advanced.abs_diff(Duration::from_millis(3_030)) < Duration::from_millis(120),
        "advanced {advanced:?} of source in about 2.02 s of output"
    );

    let clear = Arc::clone(&session);
    let remove = thread::spawn(move || clear.set_filter_graph(None));
    assert!(handle.wait_for_pending_filter(&session));
    step(1, &mut sent);
    remove.join().unwrap().unwrap();
    let removed_at = position(&session);
    step(50, &mut sent);
    let advanced = position(&session) - removed_at;
    assert!(
        advanced.abs_diff(Duration::from_millis(1_000)) < Duration::from_millis(60),
        "advanced {advanced:?} of source in 1 s of output once the tempo is removed"
    );

    handle.release(100_000);
    session.wait();
    assert_eq!(session.state(), PlaybackState::Completed);
    let end = session.position().media.unwrap().position;
    assert!(
        end.abs_diff(Duration::from_secs(8)) < Duration::from_millis(60),
        "ended at {end:?}"
    );
}
