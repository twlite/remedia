//! Resource release after staged playback ends in every way it can.
//!
//! A source's bytes are held through an `Arc` by the session, the staging worker, the decoder
//! and a handed-off decoder. The tests keep their own handle to that `Arc`, so its strong count
//! returning to one proves every decoder, stage and buffer that referenced the source was
//! dropped, without depending on timing or process-level memory figures.

use super::preskip::*;
use super::*;

/// A source whose release can be observed through the returned handle.
fn tracked(bytes: &[u8]) -> (InputSource, Arc<Vec<u8>>) {
    let bytes = Arc::new(bytes.to_vec());

    (InputSource::Bytes(Arc::clone(&bytes)), bytes)
}

/// Wait for every internal owner of `bytes` to be dropped. Only the caller's handle may remain.
fn assert_released(label: &str, bytes: &Arc<Vec<u8>>) {
    let deadline = Instant::now() + TEST_TIMEOUT;

    while Arc::strong_count(bytes) > 1 {
        assert!(
            Instant::now() < deadline,
            "{label} still has {} internal owners",
            Arc::strong_count(bytes) - 1
        );
        thread::yield_now();
    }
}

fn wait_for_staged_audio(session: &PlaybackSession, samples: usize) {
    let deadline = Instant::now() + TEST_TIMEOUT;

    while session
        .staged_source()
        .is_some_and(|stage| stage.buffered_samples < samples)
    {
        assert!(Instant::now() < deadline, "the stage never filled");
        thread::yield_now();
    }
}

fn crossfading_session() -> (PlaybackSession, StepPacketHandle, Arc<Vec<u8>>) {
    let (sink, handle) = StepPacketSink::new();
    let session = PlaybackSession::new();
    session
        .configure_crossfade(Some(
            super::super::CrossfadeConfig::new(1000, 5000).unwrap(),
        ))
        .unwrap();
    let (source, active) = tracked(SILENT_LEAD);
    session
        .play(source, OpusEncoderOutput::new(sink).unwrap())
        .unwrap();
    assert!(handle.wait_for_calls(1));

    (session, handle, active)
}

fn stage(session: &PlaybackSession, source: InputSource, entry: &str, revision: u64) {
    session
        .stage_next(
            source,
            super::super::StageIdentity {
                expected_generation: session.source_generation(),
                entry_id: entry.into(),
                queue_revision: revision,
                source_revision: 1,
            },
        )
        .unwrap();
}

#[test]
fn replacing_a_staged_source_releases_the_previous_stage() {
    let (session, handle, active) = crossfading_session();
    let (first, first_bytes) = tracked(SILENT_LEAD_LONG);
    stage(&session, first, "first", 1);
    wait_for_staged_audio(&session, 48_000);

    let (second, second_bytes) = tracked(TRANSIENTS);
    stage(&session, second, "second", 2);

    assert_released("the replaced stage", &first_bytes);
    assert!(
        Arc::strong_count(&second_bytes) > 1,
        "the new stage owns its source"
    );

    session.stop().unwrap();
    handle.release(usize::MAX);
    assert_released("the second stage after stop", &second_bytes);
    assert_released("the active source after stop", &active);
}

#[test]
fn clearing_the_staged_source_releases_its_decoder_and_buffer() {
    let (session, handle, active) = crossfading_session();
    let (next, next_bytes) = tracked(SILENT_LEAD_LONG);
    stage(&session, next, "next", 1);
    wait_for_staged_audio(&session, 48_000);

    session.clear_staged_source();

    assert!(session.staged_source().is_none());
    assert_released("the cleared stage", &next_bytes);
    assert!(
        Arc::strong_count(&active) > 1,
        "the audible source is untouched"
    );

    session.stop().unwrap();
    handle.release(usize::MAX);
    assert_released("the active source after stop", &active);
}

#[test]
fn stopping_with_a_staged_source_releases_both_sources() {
    let (session, handle, active) = crossfading_session();
    let (next, next_bytes) = tracked(SILENT_LEAD_LONG);
    stage(&session, next, "next", 1);
    wait_for_staged_audio(&session, 48_000);

    session.stop().unwrap();
    handle.release(usize::MAX);

    assert_released("the staged source", &next_bytes);
    assert_released("the active source", &active);
}

#[test]
fn replacing_the_playing_source_drops_the_stage_prepared_for_the_old_one() {
    let (session, handle, active) = crossfading_session();
    let (next, next_bytes) = tracked(SILENT_LEAD_LONG);
    stage(&session, next, "next", 1);
    wait_for_staged_audio(&session, 48_000);

    // A skip: the application plays something else instead.
    session.stop().unwrap();
    handle.release(usize::MAX);
    let replacement = NullOutput::new();
    let source = TempWave::constant(4800, 4000);
    session.play(source.source(), replacement).unwrap();

    assert_released("the stage prepared for the skipped source", &next_bytes);
    assert_released("the skipped source", &active);
    assert!(session.staged_source().is_none());
    assert_eq!(session.wait(), PlaybackState::Completed);
}

#[test]
fn a_promoted_pair_is_fully_released_after_playback_completes() {
    let (session, handle, active) = crossfading_session();
    let (next, next_bytes) = tracked(SILENT_LEAD_LONG);
    stage(&session, next, "next", 1);
    wait_for_staged_audio(&session, 96_000);

    handle.release(usize::MAX);

    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(session.source_generation(), 2, "the stage was promoted");
    assert!(session.staged_source().is_none());

    // The finished worker owns nothing; releasing the session releases the rest.
    assert_released("the finished outgoing source", &active);
    assert_released(
        "the promoted source and its handed-off decoder",
        &next_bytes,
    );
    session.stop().unwrap();
}

#[test]
fn stopping_during_the_overlap_releases_the_decoder_that_was_handed_off() {
    let (session, handle, active) = crossfading_session();
    let (next, next_bytes) = tracked(SILENT_LEAD_LONG);
    stage(&session, next, "next", 1);
    wait_for_staged_audio(&session, 96_000);

    // Into the overlap but before the promotion completes.
    handle.release(40);
    assert!(handle.wait_for_calls(41));
    session.stop().unwrap();
    handle.release(usize::MAX);

    assert_released("the incoming source", &next_bytes);
    assert_released("the outgoing source", &active);
}

#[test]
fn stopping_after_the_promotion_releases_the_promoted_decoder() {
    let (session, handle, active) = crossfading_session();
    let (next, next_bytes) = tracked(SILENT_LEAD_LONG);
    stage(&session, next, "next", 1);
    wait_for_staged_audio(&session, 96_000);

    // Past the overlap, partway through the promoted track.
    handle.release(120);
    assert!(handle.wait_for_calls(121));
    assert_eq!(session.source_generation(), 2);
    session.stop().unwrap();
    handle.release(usize::MAX);

    assert_released("the promoted source", &next_bytes);
    assert_released("the outgoing source", &active);
}

#[test]
fn a_stage_that_fails_to_decode_is_reported_and_releases_its_source() {
    let (session, handle, active) = crossfading_session();
    let (next, next_bytes) = tracked(b"this is not audio in any container");
    stage(&session, next, "broken", 1);

    let deadline = Instant::now() + TEST_TIMEOUT;
    while session
        .staged_source()
        .is_some_and(|stage| stage.error.is_none())
    {
        assert!(Instant::now() < deadline, "the stage failure is reported");
        thread::yield_now();
    }

    session.clear_staged_source();
    assert_released("the failed stage", &next_bytes);

    // The audible track is unaffected and still finishes normally.
    handle.release(usize::MAX);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(session.source_generation(), 1, "nothing was promoted");
    assert_released("the active source", &active);
    session.stop().unwrap();
}

#[test]
fn many_stage_and_promote_cycles_leave_no_owners_behind() {
    let mut sources = Vec::new();
    for cycle in 0..6 {
        let (session, handle, active) = crossfading_session();
        let (next, next_bytes) = tracked(if cycle % 2 == 0 {
            SILENT_LEAD_LONG
        } else {
            TRANSIENTS
        });
        stage(&session, next, "next", 1);
        wait_for_staged_audio(&session, 96_000);
        if cycle % 3 == 0 {
            session.clear_staged_source();
        }
        handle.release(usize::MAX);
        let _ = session.wait();
        session.stop().unwrap();
        drop(session);
        sources.push(active);
        sources.push(next_bytes);
    }

    for (index, bytes) in sources.iter().enumerate() {
        assert_released(&format!("cycle source {index}"), bytes);
    }
}

fn process_rss_kib_and_threads() -> (u64, usize) {
    let pid = std::process::id().to_string();
    let rss = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .expect("run ps");
    let rss = String::from_utf8_lossy(&rss.stdout).trim().parse().unwrap();
    let threads = std::process::Command::new("ps")
        .args(["-M", "-p", &pid])
        .output()
        .expect("run ps");
    let threads = String::from_utf8_lossy(&threads.stdout)
        .lines()
        .count()
        .saturating_sub(1);

    (rss, threads)
}

/// Steady-state check, not a pass/fail correctness test: run with
/// `cargo test --release -p remedia-core --lib steady_state -- --ignored --nocapture`.
///
/// Prints resident memory and OS thread count after every batch of stage-and-promote cycles. A
/// flat series means transitions leave nothing behind; growth would mean a leak.
#[test]
#[ignore = "measurement; prints process RSS and thread counts"]
fn steady_state_memory_and_threads_after_many_transitions() {
    let mut series = Vec::new();
    for batch in 0..8 {
        for cycle in 0..125 {
            let (session, handle, _active) = crossfading_session();
            let (next, _next) = tracked(if cycle % 2 == 0 {
                SILENT_LEAD_LONG
            } else {
                TRANSIENTS
            });
            stage(&session, next, "next", 1);
            wait_for_staged_audio(&session, 96_000);
            if cycle % 5 == 0 {
                session.clear_staged_source();
            }
            handle.release(usize::MAX);
            let _ = session.wait();
            session.stop().unwrap();
        }
        let (rss, threads) = process_rss_kib_and_threads();
        println!(
            "after {:>3} cycles: rss {rss} KiB, threads {threads}",
            (batch + 1) * 125
        );
        series.push((rss, threads));
    }

    let (_, first_threads) = series[0];
    let (_, last_threads) = series[series.len() - 1];
    assert!(
        last_threads <= first_threads + 1,
        "thread count grew from {first_threads} to {last_threads}"
    );
}
