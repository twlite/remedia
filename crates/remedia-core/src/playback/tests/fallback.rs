//! Fallback seeking: replacing the active decoder when it cannot reposition itself.
//!
//! The source is a mono WAV whose sample value is the position in milliseconds, so the audio
//! that reaches the output says exactly where in the track it came from. It is served over HTTP
//! by a server that ignores `Range`, which is what makes FFmpeg unable to seek it.

use super::*;
use crate::byte_stream::ByteStreamWriter;
use crate::playback::{CrossfadeConfig, CrossfadeEvent};
use crate::source::{ReopenError, SourceReopener};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

/// The track length in milliseconds.
const TRACK_MS: usize = 20_000;

/// A mono 48 kHz WAV where every sample of millisecond `n` has the value `n`.
fn ramp_wave() -> Vec<u8> {
    pcm_wave(
        &(0..TRACK_MS * 48)
            .map(|index| (index / 48) as i16)
            .collect::<Vec<_>>(),
    )
}

/// An HTTP server that answers every request with the whole body and never honours `Range`.
struct RangelessServer {
    url: String,
    connections: Arc<AtomicUsize>,
    /// Connections numbered at or after this are held back while `hold` is set.
    hold_after: Arc<AtomicUsize>,
    hold: Arc<AtomicBool>,
    /// Connections numbered at or after this are refused with a 503.
    fail_after: Arc<AtomicUsize>,
}

impl RangelessServer {
    fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = Self {
            url: format!("http://{address}/track.wav"),
            connections: Arc::new(AtomicUsize::new(0)),
            hold_after: Arc::new(AtomicUsize::new(usize::MAX)),
            hold: Arc::new(AtomicBool::new(false)),
            fail_after: Arc::new(AtomicUsize::new(usize::MAX)),
        };
        let connections = Arc::clone(&server.connections);
        let hold_after = Arc::clone(&server.hold_after);
        let hold = Arc::clone(&server.hold);
        let fail_after = Arc::clone(&server.fail_after);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let body = body.clone();
                let connections = Arc::clone(&connections);
                let hold_after = Arc::clone(&hold_after);
                let hold = Arc::clone(&hold);
                let fail_after = Arc::clone(&fail_after);

                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    loop {
                        let mut header = String::new();
                        let read = reader.read_line(&mut header).unwrap_or(0);
                        if read == 0 || header == "\r\n" {
                            break;
                        }
                    }

                    let index = connections.fetch_add(1, Ordering::SeqCst);
                    if index >= fail_after.load(Ordering::SeqCst) {
                        let _ = stream.write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        return;
                    }
                    while index >= hold_after.load(Ordering::SeqCst) && hold.load(Ordering::SeqCst)
                    {
                        thread::sleep(Duration::from_millis(2));
                    }

                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    if stream.write_all(header.as_bytes()).is_ok() {
                        let _ = stream.write_all(&body);
                    }
                });
            }
        });

        server
    }

    fn source(&self) -> InputSource {
        InputSource::http_url(&self.url)
    }

    /// Hold back every connection opened after the ones made so far.
    fn hold_new_connections(&self) {
        self.hold_after
            .store(self.connections.load(Ordering::SeqCst), Ordering::SeqCst);
        self.hold.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        self.hold.store(false, Ordering::SeqCst);
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn wait_for_connections(&self, count: usize) -> bool {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while self.connections() < count {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
        true
    }
}

/// Position in milliseconds that the ramp encodes at the start of an accepted frame. Mono is
/// mixed into stereo at -3 dB, which the conversion undoes.
fn ms_of(first_sample: f32) -> i64 {
    (first_sample * 32768.0 * std::f32::consts::SQRT_2).round() as i64
}

/// Whether `next` continues `previous` without a gap or repeat, to within rounding to a whole
/// millisecond. Frames are not a whole number of milliseconds long.
fn continues(previous: &(u64, i64, usize), next: &(u64, i64, usize)) -> bool {
    (next.1 - (previous.1 + previous.2 as i64 / 48)).abs() <= 1
}

/// What the output received, as `(start_sample, source ms at the frame's start, samples)`.
fn delivered(gate: &GateHandle) -> Vec<(u64, i64, usize)> {
    let accepted = gate.snapshot().accepted;
    let lengths = frame_lengths(&accepted);

    accepted
        .iter()
        .zip(lengths)
        .map(|((start, first), length)| (*start, ms_of(*first), length))
        .collect()
}

/// Frame lengths follow from the output sample positions, except for the last frame.
fn frame_lengths(accepted: &[(u64, f32)]) -> Vec<usize> {
    let mut lengths: Vec<usize> = accepted
        .windows(2)
        .map(|pair| (pair[1].0 - pair[0].0) as usize)
        .collect();
    lengths.push(*lengths.last().unwrap_or(&960));
    lengths
}

/// Let the worker deliver `frames` more frames, one at a time.
fn consume(gate: &GateHandle, frames: usize) {
    for _ in 0..frames {
        let target = gate.snapshot().accepted.len() + 1;
        gate.release(1);
        assert!(gate.wait_for_accepted(target), "worker stopped delivering");
    }
}

fn assert_applied(result: Result<(), PlaybackError>) {
    assert!(
        result.is_ok(),
        "the seek should have been applied: {result:?}"
    );
}

/// Request a seek while the worker is blocked writing a frame, so the playhead it starts from is
/// the end of the last frame the output accepted plus the one it is holding.
fn start_seek(
    session: &Arc<PlaybackSession>,
    gate: &GateHandle,
    milliseconds: u64,
) -> thread::JoinHandle<Result<(), PlaybackError>> {
    assert!(gate.wait_for_calls(gate.snapshot().accepted.len() + 1));
    let session = Arc::clone(session);
    thread::spawn(move || session.seek(Duration::from_millis(milliseconds)))
}

/// Keep the output draining until `seek` returns.
fn pump_until<T>(gate: &GateHandle, seek: thread::JoinHandle<T>) -> T {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !seek.is_finished() {
        assert!(Instant::now() < deadline, "the seek never completed");
        gate.release(1);
        thread::sleep(Duration::from_millis(1));
    }
    seek.join().expect("seek thread joins")
}

/// The index of the one frame where the source position jumps by more than `threshold` ms.
fn find_jump(frames: &[(u64, i64, usize)], threshold: i64) -> usize {
    let jumps: Vec<usize> = (1..frames.len())
        .filter(|index| (frames[*index].1 - frames[index - 1].1).abs() > threshold)
        .collect();
    assert_eq!(jumps.len(), 1, "expected exactly one seek, found {jumps:?}");
    jumps[0]
}

/// Where the seek cut the old source, in milliseconds on its timeline: the end of the last frame
/// that is purely the old audio. The frame after it carries the 5 ms blend, so its first sample
/// is not a clean position reading.
fn cut_at(frames: &[(u64, i64, usize)], jump: usize) -> i64 {
    frames[jump - 2].1 + frames[jump - 2].2 as i64 / 48
}

/// Where the replacement began, in milliseconds on the track's timeline. The frame before
/// `jump` is the blend frame; the new source's audio is where the next frame says it should be.
fn resumed_at(frames: &[(u64, i64, usize)], jump: usize) -> i64 {
    frames[jump].1 - frames[jump - 1].2 as i64 / 48
}

fn assert_output_timeline_is_contiguous(frames: &[(u64, i64, usize)]) {
    for pair in frames.windows(2) {
        assert_eq!(
            pair[1].0,
            pair[0].0 + pair[0].2 as u64,
            "the output timeline must not jump or repeat"
        );
    }
}

/// A fallback-enabled session playing the range-less track, paused on its first frame's write.
fn playing(server: &RangelessServer) -> (Arc<PlaybackSession>, GateHandle) {
    let session = Arc::new(PlaybackSession::new());
    session.configure_fallback_seeking(true);
    let (output, gate) = GateOutput::new();
    session.play(server.source(), output).expect("track opens");

    (session, gate)
}

#[test]
fn fallback_seeking_is_disabled_by_default() {
    assert!(!PlaybackSession::new().fallback_seeking());

    let session = PlaybackSession::new();
    session.configure_fallback_seeking(true);
    assert!(session.fallback_seeking());
    session.configure_fallback_seeking(false);
    assert!(!session.fallback_seeking());
}

#[test]
fn without_the_option_a_source_that_cannot_rewind_still_reports_unsupported_seeking() {
    let wave = ramp_wave();
    let (input, writer) = InputSource::byte_stream(16 * 1024).unwrap();
    let feeder = thread::spawn(move || {
        for chunk in wave.chunks(4096) {
            if writer.write(chunk).is_err() {
                return;
            }
        }
        let _ = writer.finish();
    });
    let reopener = Arc::new(CountingReopener {
        calls: AtomicUsize::new(0),
        wave: ramp_wave(),
    });

    let session = Arc::new(PlaybackSession::new());
    assert!(!session.fallback_seeking());
    let (output, gate) = GateOutput::new();
    session
        .play_with_reopener(
            input,
            Arc::clone(&reopener) as Arc<dyn SourceReopener>,
            output,
            CancellationToken::new(),
        )
        .unwrap();
    consume(&gate, 60);

    let seek = start_seek(&session, &gate, 100);
    let result = pump_until(&gate, seek);

    assert!(
        matches!(result, Err(PlaybackError::UnsupportedSeeking(_))),
        "{result:?}"
    );
    assert_eq!(reopener.calls.load(Ordering::SeqCst), 0);
    assert_eq!(session.state(), PlaybackState::Playing);
    session.stop().unwrap();
    let _ = feeder.join();
}

#[test]
fn the_replacement_resumes_where_the_listener_would_be_by_now() {
    let server = RangelessServer::start(ramp_wave());
    let (session, gate) = playing(&server);
    consume(&gate, 20);
    server.hold_new_connections();

    let seek = start_seek(&session, &gate, 10_000);
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    let at_request = gate.snapshot().accepted.len();
    assert!(server.wait_for_connections(2), "a second input was opened");

    // The seek is outstanding and the replacement cannot finish. The track plays on.
    consume(&gate, 40);
    assert!(!seek.is_finished(), "the seek waits for its replacement");
    assert_eq!(session.state(), PlaybackState::Playing);

    server.release();
    assert_applied(pump_until(&gate, seek));
    consume(&gate, 10);

    let frames = delivered(&gate);
    let jump = find_jump(&frames, 1_000);
    assert!(jump > at_request + 40, "the old track kept playing: {jump}");
    assert_output_timeline_is_contiguous(&frames);

    // No audio of the old track is lost or repeated before the switch.
    for pair in frames[..jump - 1].windows(2) {
        assert!(continues(&pair[0], &pair[1]), "{pair:?}");
    }

    // The old track advanced by `elapsed` while the replacement was prepared, so the
    // replacement starts that far past the 10 s that was asked for.
    let origin = frames[at_request - 1].1 + frames[at_request - 1].2 as i64 / 48;
    let elapsed = cut_at(&frames, jump) - origin;
    assert!(
        elapsed >= 800,
        "the preparation spanned playback: {elapsed}"
    );
    let resumed = resumed_at(&frames, jump);
    assert!(
        (resumed - (10_000 + elapsed)).abs() <= 2,
        "resumed at {resumed} ms, wanted {}",
        10_000 + elapsed
    );

    // After the switch the new source plays on without a gap.
    for pair in frames[jump..].windows(2) {
        assert!(continues(&pair[0], &pair[1]), "{pair:?}");
    }

    assert_eq!(
        session.duration(),
        Some(Duration::from_millis(TRACK_MS as u64))
    );
    session.stop().unwrap();
}

#[test]
fn an_instantly_prepared_replacement_resumes_at_the_requested_position() {
    let server = RangelessServer::start(ramp_wave());
    let (session, gate) = playing(&server);
    consume(&gate, 5);

    let seek = start_seek(&session, &gate, 12_000);
    assert_applied(pump_until(&gate, seek));
    consume(&gate, 5);

    let frames = delivered(&gate);
    let jump = find_jump(&frames, 1_000);

    // Some playback happened while the second input opened, so it is not exactly 12 s, but it
    // is never before it and never more than that playback away.
    let resumed = resumed_at(&frames, jump);
    let played = cut_at(&frames, jump);
    assert!(resumed >= 12_000, "{resumed}");
    assert!(resumed - 12_000 <= played + 100, "{resumed} {played}");
    session.stop().unwrap();
}

#[test]
fn a_paused_session_does_not_advance_the_replacement() {
    let server = RangelessServer::start(ramp_wave());
    let (session, gate) = playing(&server);
    pause_at_first_blocked_frame(&session, &gate);
    let before = gate.snapshot().accepted.len();

    // Preparation takes as long as it likes: nothing is playing, so no source time passes.
    server.hold_new_connections();
    let paused_session = Arc::clone(&session);
    let seek = thread::spawn(move || paused_session.seek(Duration::from_millis(7_000)));
    assert!(server.wait_for_connections(2));
    thread::sleep(Duration::from_millis(100));
    assert!(!seek.is_finished());
    server.release();

    let deadline = Instant::now() + TEST_TIMEOUT;
    while !seek.is_finished() {
        assert!(Instant::now() < deadline, "the paused seek never completed");
        thread::sleep(Duration::from_millis(2));
    }
    assert_applied(seek.join().unwrap());
    assert_eq!(session.state(), PlaybackState::Paused);
    assert_eq!(
        gate.snapshot().accepted.len(),
        before,
        "nothing was delivered while paused"
    );

    session.resume().unwrap();
    consume(&gate, 3);
    let frames = delivered(&gate);
    let resumed = frames[before + 1].1 - frames[before].2 as i64 / 48;
    assert!(
        (resumed - 7_000).abs() <= 1,
        "no source time passed while paused, so the replacement starts on the target: {resumed}"
    );
    session.stop().unwrap();
}

#[test]
fn a_tempo_filter_scales_the_compensation_by_source_time() {
    let server = RangelessServer::start(ramp_wave());
    let (session, gate) = playing(&server);
    // The graph is applied by the worker, which is blocked writing until the output drains.
    consume(&gate, 3);
    let tempo = Arc::clone(&session);
    let apply = thread::spawn(move || tempo.set_filter_graph(Some("atempo=1.5")));
    assert!(wait_for_pending_filter_update(&session));
    assert!(pump_until(&gate, apply).is_ok());
    consume(&gate, 10);
    server.hold_new_connections();

    let seek = start_seek(&session, &gate, 5_000);
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    let at_request = gate.snapshot().accepted.len();
    assert!(server.wait_for_connections(2));
    consume(&gate, 30);
    server.release();
    assert_applied(pump_until(&gate, seek));
    consume(&gate, 10);

    let frames = delivered(&gate);
    // The tempo graph smears the cut over a frame or two: it holds a little of the old audio and
    // overlaps it with the new. The first jump marks where that starts.
    let jump = (1..frames.len())
        .find(|index| (frames[*index].1 - frames[index - 1].1).abs() > 1_000)
        .expect("the seek shows as a jump in the source position");
    let output_ms = |from: usize, to: usize| -> i64 {
        frames[from..to]
            .iter()
            .map(|frame| frame.2 as i64)
            .sum::<i64>()
            / 48
    };

    // The ramp counts source milliseconds, so the filter's output advances through it one and a
    // half times as fast as the output clock. The replacement is placed by source time.
    let origin = frames[at_request].1;
    let source_elapsed = cut_at(&frames, jump) - origin;
    let output_elapsed = output_ms(at_request, jump - 1);
    assert!(
        source_elapsed as f64 > output_elapsed as f64 * 1.3,
        "source time {source_elapsed} ms should outrun output time {output_elapsed} ms"
    );

    // Three frames on, the output is the new source alone, and each output frame spans 1.5 times
    // its length of source time. Going back to the start of the new audio is exact to a frame.
    let span = |from: usize, to: usize| -> f64 {
        frames[from..to]
            .iter()
            .map(|frame| frame.2 as f64 * 1.5 / 48.0)
            .sum()
    };
    let resumed = frames[jump + 3].1 as f64 - span(jump, jump + 3);
    let wanted = 5_000.0 + source_elapsed as f64;
    assert!(
        (resumed - wanted).abs() <= 120.0,
        "resumed at {resumed} ms, wanted about {wanted}"
    );
    session.stop().unwrap();
}

#[test]
fn only_the_latest_of_rapid_consecutive_seeks_replaces_the_source() {
    let server = RangelessServer::start(ramp_wave());
    let (session, gate) = playing(&server);
    consume(&gate, 5);
    server.hold_new_connections();

    let first = start_seek(&session, &gate, 6_000);
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    assert!(
        server.wait_for_connections(2),
        "the first seek is preparing"
    );

    let second = start_seek(&session, &gate, 14_000);
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    // The newer seek makes the older one obsolete straight away.
    assert!(matches!(
        pump_until(&gate, first),
        Err(PlaybackError::SeekSuperseded)
    ));

    assert!(
        server.wait_for_connections(3),
        "the latest seek is preparing"
    );
    server.release();
    assert_applied(pump_until(&gate, second));
    consume(&gate, 5);

    let frames = delivered(&gate);
    let jump = find_jump(&frames, 1_000);
    assert!(
        resumed_at(&frames, jump) >= 14_000,
        "{}",
        resumed_at(&frames, jump)
    );
    assert!(
        frames
            .iter()
            .all(|frame| !(6_000..14_000).contains(&frame.1) || frame.1 < 1_000),
        "audio from the superseded seek must never play"
    );
    session.stop().unwrap();
}

#[test]
fn a_source_that_fails_to_reopen_leaves_playback_untouched() {
    let server = RangelessServer::start(ramp_wave());
    let (session, gate) = playing(&server);
    consume(&gate, 5);
    server
        .fail_after
        .store(server.connections(), Ordering::SeqCst);

    let seek = start_seek(&session, &gate, 9_000);
    let result = pump_until(&gate, seek);

    assert!(result.is_err(), "{result:?}");
    assert!(
        !matches!(result, Err(PlaybackError::SeekSuperseded)),
        "{result:?}"
    );
    assert_eq!(session.state(), PlaybackState::Playing);

    // The original stream goes on from where it was, then reaches its natural end.
    gate.release(2_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    let frames = delivered(&gate);
    for pair in frames.windows(2) {
        assert!(continues(&pair[0], &pair[1]), "{pair:?}");
    }
    assert!(frames.last().unwrap().1 > (TRACK_MS as i64 - 200));
}

#[test]
fn seeking_past_the_end_of_the_source_fails_without_touching_playback() {
    let server = RangelessServer::start(ramp_wave());
    let (session, gate) = playing(&server);
    consume(&gate, 5);

    // The duration check in `seek` rejects this before any replacement is attempted.
    let result = session.seek(Duration::from_millis(TRACK_MS as u64 + 5_000));

    assert!(matches!(result, Err(PlaybackError::SeekOutOfRange { .. })));
    assert_eq!(server.connections(), 1);
    assert_eq!(session.state(), PlaybackState::Playing);
    session.stop().unwrap();
}

#[test]
fn stopping_cancels_a_preparation_that_has_not_finished() {
    let server = RangelessServer::start(ramp_wave());
    let (session, gate) = playing(&server);
    consume(&gate, 5);
    server.hold_new_connections();

    let seek = start_seek(&session, &gate, 10_000);
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    assert!(server.wait_for_connections(2));

    let started = Instant::now();
    session.stop().unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "stop must not wait for the held connection"
    );
    assert!(matches!(
        seek.join().unwrap(),
        Err(PlaybackError::Cancelled)
    ));
    server.release();
}

#[test]
fn replacing_the_track_cancels_the_pending_seek_and_a_new_playback_is_unaffected() {
    let server = RangelessServer::start(ramp_wave());
    let (session, gate) = playing(&server);
    consume(&gate, 5);
    server.hold_new_connections();

    let seek = start_seek(&session, &gate, 10_000);
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    assert!(server.wait_for_connections(2));
    session.stop().unwrap();
    assert!(seek.join().unwrap().is_err());
    server.release();

    let (output, gate) = GateOutput::new();
    session.play(server.source(), output).unwrap();
    consume(&gate, 3);
    let frames = delivered(&gate);
    assert!(
        frames.iter().all(|frame| frame.1 < 1_000),
        "a stale replacement must not reach the next playback"
    );
    session.stop().unwrap();
}

struct CountingReopener {
    calls: AtomicUsize,
    wave: Vec<u8>,
}

impl SourceReopener for CountingReopener {
    fn reopen(&self, _cancellation: &CancellationToken) -> Result<InputSource, ReopenError> {
        self.calls.fetch_add(1, Ordering::SeqCst);

        Ok(InputSource::bytes(self.wave.clone()))
    }
}

#[test]
fn a_byte_stream_with_a_reopener_can_seek_backwards_through_a_replacement() {
    let wave = ramp_wave();
    let (input, writer) = InputSource::byte_stream(16 * 1024).unwrap();
    let feeder_wave = wave.clone();
    let feeder = thread::spawn(move || {
        for chunk in feeder_wave.chunks(4096) {
            if writer.write(chunk).is_err() {
                return;
            }
        }
        let _ = writer.finish();
    });
    let reopener = Arc::new(CountingReopener {
        calls: AtomicUsize::new(0),
        wave,
    });

    let session = Arc::new(PlaybackSession::new());
    session.configure_fallback_seeking(true);
    let (output, gate) = GateOutput::new();
    session
        .play_with_reopener(
            input,
            Arc::clone(&reopener) as Arc<dyn SourceReopener>,
            output,
            CancellationToken::new(),
        )
        .unwrap();
    consume(&gate, 60);

    // A one-shot stream cannot rewind, so this can only be served by a second input.
    let seek = start_seek(&session, &gate, 100);
    assert_applied(pump_until(&gate, seek));
    consume(&gate, 5);

    assert_eq!(reopener.calls.load(Ordering::SeqCst), 1);
    let frames = delivered(&gate);
    let jump = find_jump(&frames, 500);
    let resumed = resumed_at(&frames, jump);
    assert!((100..1_500).contains(&resumed), "{resumed}");
    assert!(resumed < cut_at(&frames, jump), "it moved backwards");

    session.stop().unwrap();
    let _ = feeder.join();
}

#[test]
fn a_byte_stream_without_a_reopener_keeps_the_unsupported_seek_error() {
    let wave = ramp_wave();
    let (input, writer) = InputSource::byte_stream(16 * 1024).unwrap();
    let feeder = thread::spawn(move || {
        for chunk in wave.chunks(4096) {
            if writer.write(chunk).is_err() {
                return;
            }
        }
        let _ = writer.finish();
    });

    let session = Arc::new(PlaybackSession::new());
    session.configure_fallback_seeking(true);
    let (output, gate) = GateOutput::new();
    session.play(input, output).unwrap();
    consume(&gate, 60);

    let seek = start_seek(&session, &gate, 100);
    let result = pump_until(&gate, seek);

    assert!(
        matches!(result, Err(PlaybackError::UnsupportedSeeking(_))),
        "{result:?}"
    );
    assert_eq!(session.state(), PlaybackState::Playing);
    session.stop().unwrap();
    let _ = feeder.join();
}

#[test]
fn a_failing_reopener_leaves_the_stream_playing() {
    struct Refusing;

    impl SourceReopener for Refusing {
        fn reopen(&self, _cancellation: &CancellationToken) -> Result<InputSource, ReopenError> {
            Err(ReopenError::new("the grant could not be issued again"))
        }
    }

    let wave = ramp_wave();
    let (input, writer) = InputSource::byte_stream(16 * 1024).unwrap();
    let feeder = thread::spawn(move || {
        for chunk in wave.chunks(4096) {
            if writer.write(chunk).is_err() {
                return;
            }
        }
        let _ = writer.finish();
    });

    let session = Arc::new(PlaybackSession::new());
    session.configure_fallback_seeking(true);
    let (output, gate) = GateOutput::new();
    session
        .play_with_reopener(input, Arc::new(Refusing), output, CancellationToken::new())
        .unwrap();
    consume(&gate, 60);

    let seek = start_seek(&session, &gate, 100);
    let result = pump_until(&gate, seek);

    match result {
        Err(PlaybackError::UnsupportedSeeking(message)) => {
            assert!(
                message.contains("grant could not be issued again"),
                "{message}"
            );
        }
        other => panic!("unexpected result {other:?}"),
    }
    assert_eq!(session.state(), PlaybackState::Playing);
    session.stop().unwrap();
    let _ = feeder.join();
}

#[test]
fn a_slow_reopener_is_cancelled_by_stop() {
    struct Slow(Arc<AtomicBool>);

    impl SourceReopener for Slow {
        fn reopen(&self, cancellation: &CancellationToken) -> Result<InputSource, ReopenError> {
            self.0.store(true, Ordering::SeqCst);
            while !cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }

            Err(ReopenError::new("cancelled"))
        }
    }

    let wave = ramp_wave();
    let (input, writer) = InputSource::byte_stream(16 * 1024).unwrap();
    let feeder = thread::spawn(move || {
        for chunk in wave.chunks(4096) {
            if writer.write(chunk).is_err() {
                return;
            }
        }
        let _ = writer.finish();
    });
    let entered = Arc::new(AtomicBool::new(false));

    let session = Arc::new(PlaybackSession::new());
    session.configure_fallback_seeking(true);
    let (output, gate) = GateOutput::new();
    session
        .play_with_reopener(
            input,
            Arc::new(Slow(Arc::clone(&entered))),
            output,
            CancellationToken::new(),
        )
        .unwrap();
    consume(&gate, 5);

    let seek = start_seek(&session, &gate, 100);
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !entered.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the reopener was never called");
        thread::sleep(Duration::from_millis(2));
    }

    let started = Instant::now();
    session.stop().unwrap();

    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(matches!(
        seek.join().unwrap(),
        Err(PlaybackError::Cancelled)
    ));
    let _ = feeder.join();
}

fn crossfading(
    server: &RangelessServer,
    config: CrossfadeConfig,
) -> (Arc<PlaybackSession>, GateHandle) {
    let session = Arc::new(PlaybackSession::new());
    session.configure_crossfade(Some(config)).unwrap();
    session.configure_fallback_seeking(true);
    let (output, gate) = GateOutput::new();
    session.play(server.source(), output).expect("track opens");

    (session, gate)
}

#[test]
fn the_crossfade_worker_replaces_the_source_and_publishes_the_new_timeline() {
    let server = RangelessServer::start(ramp_wave());
    let (session, gate) = crossfading(&server, CrossfadeConfig::new(1_000, 5_000).unwrap());
    consume(&gate, 20);
    server.hold_new_connections();

    let seek = start_seek(&session, &gate, 10_000);
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    let at_request = gate.snapshot().accepted.len();
    assert!(server.wait_for_connections(2));
    consume(&gate, 30);
    let generation = session.source_generation();
    server.release();
    assert_applied(pump_until(&gate, seek));
    consume(&gate, 10);

    let frames = delivered(&gate);
    let jump = find_jump(&frames, 1_000);
    assert_output_timeline_is_contiguous(&frames);
    let origin = frames[at_request - 1].1 + frames[at_request - 1].2 as i64 / 48;
    let wanted = 10_000 + cut_at(&frames, jump) - origin;
    assert!(
        (resumed_at(&frames, jump) - wanted).abs() <= 2,
        "resumed at {}, wanted {wanted}",
        resumed_at(&frames, jump)
    );

    assert!(
        session.source_generation() > generation,
        "the timeline changed"
    );
    assert!(
        session
            .take_crossfade_events()
            .iter()
            .any(|event| matches!(event, CrossfadeEvent::TimelineChanged { .. }))
    );
    session.stop().unwrap();
}

#[test]
fn a_promoted_source_that_cannot_seek_is_replaced_like_the_first() {
    let first = RangelessServer::start(ramp_wave());
    let second = RangelessServer::start(ramp_wave());
    let (session, gate) = crossfading(&first, CrossfadeConfig::new(1_000, 5_000).unwrap());
    let identity = crate::playback::StageIdentity {
        expected_generation: session.source_generation(),
        entry_id: "second".to_owned(),
        queue_revision: 1,
        source_revision: 0,
    };
    session.stage_next(second.source(), identity).unwrap();
    let deadline = Instant::now() + TEST_TIMEOUT;
    while session.staged_source().map(|stage| stage.state)
        != Some(crate::playback::StageState::Ready)
    {
        assert!(
            Instant::now() < deadline,
            "the second track never became ready"
        );
        thread::sleep(Duration::from_millis(5));
    }

    // Play the first track through its overlap until the second one is the active source.
    let completed = |session: &PlaybackSession| {
        session
            .take_crossfade_events()
            .iter()
            .any(|event| matches!(event, CrossfadeEvent::TransitionCompleted { .. }))
    };
    let deadline = Instant::now() + TEST_TIMEOUT;
    let mut finished = false;
    while !finished {
        assert!(Instant::now() < deadline, "the transition never completed");
        consume(&gate, 1);
        finished = completed(&session);
    }
    consume(&gate, 5);
    let before = gate.snapshot().accepted.len();
    second.hold_new_connections();

    let seek = start_seek(&session, &gate, 8_000);
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    assert!(
        second.wait_for_connections(2),
        "the promoted track was opened again"
    );
    consume(&gate, 30);
    second.release();
    assert_applied(pump_until(&gate, seek));
    consume(&gate, 10);

    let frames = delivered(&gate);
    let frames = &frames[before..];
    let jump = find_jump(frames, 1_000);
    let origin = frames[0].1 + frames[0].2 as i64 / 48;
    let wanted = 8_000 + cut_at(frames, jump) - origin;
    assert!(
        (resumed_at(frames, jump) - wanted).abs() <= 25,
        "resumed at {}, wanted about {wanted}",
        resumed_at(frames, jump)
    );
    session.stop().unwrap();
}

#[test]
fn a_crossfade_that_begins_first_supersedes_the_pending_seek() {
    let server = RangelessServer::start(ramp_wave());
    let (session, gate) = crossfading(&server, CrossfadeConfig::new(1_000, 5_000).unwrap());
    consume(&gate, 5);
    server.hold_new_connections();

    let seek = start_seek(&session, &gate, 3_000);
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    assert!(server.wait_for_connections(2));

    // Stage the next track, then play the current one into its overlap.
    let next = RangelessServer::start(ramp_wave());
    let identity = crate::playback::StageIdentity {
        expected_generation: session.source_generation(),
        entry_id: "next".to_owned(),
        queue_revision: 1,
        source_revision: 0,
    };
    session.stage_next(next.source(), identity).unwrap();
    let deadline = Instant::now() + TEST_TIMEOUT;
    while session.staged_source().map(|stage| stage.state)
        != Some(crate::playback::StageState::Ready)
    {
        assert!(
            Instant::now() < deadline,
            "the next track never became ready"
        );
        thread::sleep(Duration::from_millis(5));
    }

    let result = pump_until(&gate, seek);

    assert!(
        matches!(result, Err(PlaybackError::SeekSuperseded)),
        "{result:?}"
    );
    server.release();
    gate.release(5_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
}

/// A one-shot stream of `wave` fed by a thread, and a reopener that hands out `wave` again.
fn byte_stream_with_reopener(
    wave: &[u8],
) -> (
    InputSource,
    ByteStreamWriter,
    thread::JoinHandle<()>,
    Arc<CountingReopener>,
) {
    let (input, writer) = InputSource::byte_stream(16 * 1024).unwrap();
    let feeder_writer = writer.clone();
    let feeder_wave = wave.to_vec();
    let feeder = thread::spawn(move || {
        for chunk in feeder_wave.chunks(4096) {
            if feeder_writer.write(chunk).is_err() {
                return;
            }
        }
        let _ = feeder_writer.finish();
    });
    let reopener = Arc::new(CountingReopener {
        calls: AtomicUsize::new(0),
        wave: wave.to_vec(),
    });

    (input, writer, feeder, reopener)
}

#[test]
fn a_seek_can_be_submitted_without_waiting_and_a_newer_one_replaces_it() {
    let (input, writer, feeder, reopener) = byte_stream_with_reopener(&ramp_wave());
    let session = Arc::new(PlaybackSession::new());
    session.configure_fallback_seeking(true);
    let (output, gate) = GateOutput::new();
    session
        .play_with_reopener(
            input,
            Arc::clone(&reopener) as Arc<dyn SourceReopener>,
            output,
            CancellationToken::new(),
        )
        .unwrap();
    consume(&gate, 60);
    assert!(gate.wait_for_calls(gate.snapshot().accepted.len() + 1));

    // Submitting returns at once, even though the seek cannot be applied until the worker runs.
    let first = session.start_seek(Duration::from_millis(100)).unwrap();
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    let deadline = Instant::now() + TEST_TIMEOUT;
    while lock(&session.shared.data).pending_seek.is_some() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }

    // The worker has taken the first one, so a second can be submitted straight away.
    let second = session.start_seek(Duration::from_millis(300)).unwrap();
    let waiter = thread::spawn(move || first.wait());
    assert!(matches!(
        pump_until(&gate, waiter),
        Err(PlaybackError::SeekSuperseded)
    ));
    let second = thread::spawn(move || second.wait());
    assert_applied(pump_until(&gate, second));

    session.stop().unwrap();
    drop(writer);
    let _ = feeder.join();
}

#[test]
fn a_swap_releases_the_old_source_and_the_preparation_thread() {
    let wave = ramp_wave();
    let (input, writer, feeder, reopener) = byte_stream_with_reopener(&wave);
    let session = Arc::new(PlaybackSession::new());
    session.configure_fallback_seeking(true);
    let (output, gate) = GateOutput::new();
    session
        .play_with_reopener(
            input,
            Arc::clone(&reopener) as Arc<dyn SourceReopener>,
            output,
            CancellationToken::new(),
        )
        .unwrap();
    consume(&gate, 60);
    assert!(!writer.is_closed());

    let seek = start_seek(&session, &gate, 100);
    assert_applied(pump_until(&gate, seek));
    consume(&gate, 3);

    // The old decoder was dropped by the swap, which closes its stream and unblocks its feeder.
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !writer.is_closed() {
        assert!(
            Instant::now() < deadline,
            "the old stream was never released"
        );
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(reopener.calls.load(Ordering::SeqCst), 1);

    session.stop().unwrap();
    assert_eq!(session.state(), PlaybackState::Stopped);
    let _ = feeder.join();
}

struct Waiting {
    entered: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl SourceReopener for Waiting {
    fn reopen(&self, cancellation: &CancellationToken) -> Result<InputSource, ReopenError> {
        self.entered.store(true, Ordering::SeqCst);
        while !cancellation.is_cancelled() {
            thread::sleep(Duration::from_millis(2));
        }
        self.cancelled.store(true, Ordering::SeqCst);

        Err(ReopenError::new("cancelled"))
    }
}

#[test]
fn dropping_the_session_cancels_a_preparation_in_progress() {
    let wave = ramp_wave();
    let (input, writer) = InputSource::byte_stream(16 * 1024).unwrap();
    let feeder = thread::spawn(move || {
        for chunk in wave.chunks(4096) {
            if writer.write(chunk).is_err() {
                return;
            }
        }
        let _ = writer.finish();
    });
    let entered = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));

    let session = Arc::new(PlaybackSession::new());
    session.configure_fallback_seeking(true);
    let (output, gate) = GateOutput::new();
    session
        .play_with_reopener(
            input,
            Arc::new(Waiting {
                entered: Arc::clone(&entered),
                cancelled: Arc::clone(&cancelled),
            }),
            output,
            CancellationToken::new(),
        )
        .unwrap();
    consume(&gate, 5);
    let pending = {
        assert!(gate.wait_for_calls(gate.snapshot().accepted.len() + 1));
        session.start_seek(Duration::from_millis(100)).unwrap()
    };
    consume(&gate, 1);
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !entered.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the reopener was never called");
        thread::sleep(Duration::from_millis(2));
    }

    // Destroying the session stops the worker, which cancels and joins the preparation.
    let waiter = thread::spawn(move || pending.wait());
    drop(session);

    assert!(
        cancelled.load(Ordering::SeqCst),
        "the preparation was cancelled"
    );
    assert!(matches!(
        waiter.join().unwrap(),
        Err(PlaybackError::Cancelled)
    ));
    let _ = feeder.join();
}

#[test]
fn a_lost_output_fails_the_seek_and_cancels_the_preparation() {
    let wave = ramp_wave();
    let (input, writer) = InputSource::byte_stream(16 * 1024).unwrap();
    let feeder = thread::spawn(move || {
        for chunk in wave.chunks(4096) {
            if writer.write(chunk).is_err() {
                return;
            }
        }
        let _ = writer.finish();
    });
    let entered = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));

    let session = Arc::new(PlaybackSession::new());
    session.configure_fallback_seeking(true);
    let (output, gate) = GateOutput::new();
    session
        .play_with_reopener(
            input,
            Arc::new(Waiting {
                entered: Arc::clone(&entered),
                cancelled: Arc::clone(&cancelled),
            }),
            output,
            CancellationToken::new(),
        )
        .unwrap();
    consume(&gate, 5);
    let seek = start_seek(&session, &gate, 100);
    assert!(wait_for_pending_seek(&session));
    consume(&gate, 1);
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !entered.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the reopener was never called");
        thread::sleep(Duration::from_millis(2));
    }

    // The voice connection drops while the replacement is still being prepared.
    gate.disconnect();

    let result = seek.join().unwrap();
    assert!(result.is_err(), "{result:?}");
    assert_eq!(session.wait(), PlaybackState::Failed);
    assert!(
        cancelled.load(Ordering::SeqCst),
        "the preparation was cancelled"
    );
    let _ = feeder.join();
}

mod skipping {
    //! Dropping compressed packets before the target, instead of decoding them.

    use super::super::preskip::{TRANSIENTS, reference_pcm};
    use super::*;
    use crate::decoder::DecoderOptions;
    use crate::playback::fallback::{SETTLE_SAMPLES, frame_start, trim_front};

    /// Decode `bytes` after dropping packets before `target`, and return the samples from
    /// `target` onwards (stereo interleaved) together with how many packets were dropped.
    fn decode_after_skipping(bytes: &[u8], target: u64, margin: u64) -> (Vec<f32>, u64) {
        let mut decoder = AudioDecoder::open_for_playback(
            InputSource::bytes(bytes.to_vec()),
            DecoderOptions::default(),
        )
        .unwrap();
        let skipped = decoder.skip_packets_before(target, margin).unwrap();
        let mut samples = Vec::new();
        while let Some(mut frame) = decoder.next_timed_frame().unwrap() {
            let start = frame_start(&frame);
            if start + frame.frame.samples_per_channel as u64 <= target {
                continue;
            }
            trim_front(&mut frame, target.saturating_sub(start) as usize);
            samples.extend_from_slice(&frame.frame.samples);
        }

        (samples, skipped)
    }

    #[test]
    fn dropping_packets_unread_decodes_the_same_audio_as_decoding_from_the_start() {
        let reference = reference_pcm(TRANSIENTS);
        let target = 3 * 48_000;
        assert!(
            reference.len() / 2 > 5 * 48_000,
            "the fixture is long enough"
        );

        let (skipping, skipped) = decode_after_skipping(TRANSIENTS, target, SETTLE_SAMPLES);
        assert!(
            skipped > 50,
            "packets before the margin were not decoded: {skipped}"
        );

        // After the settle margin, the decoder that started late matches the one that did not.
        let compared = (reference.len() - target as usize * 2).min(skipping.len());
        assert!(compared > 2 * 48_000);
        let worst = (0..compared)
            .map(|index| (skipping[index] - reference[target as usize * 2 + index]).abs())
            .fold(0.0, f32::max);
        assert!(worst < 1e-4, "largest difference {worst}");
    }

    #[test]
    fn a_target_inside_the_margin_drops_nothing() {
        let (_, skipped) = decode_after_skipping(TRANSIENTS, 20_000, SETTLE_SAMPLES);

        assert_eq!(skipped, 0);
    }
}

mod passthrough {
    //! An Opus stream forwarded as packets is converted to PCM for the swap, without a gap.

    use super::super::preskip::{
        TRANSIENTS, assert_timeline_is_contiguous, client_pcm, reference_pcm, source_packets,
    };
    use super::*;
    use crate::opus::{EncodedOpusPacket, OpusEncoderOutput};

    /// The media sample in `reference` (stereo interleaved) whose surroundings best match
    /// `window`, searched at millisecond steps and then refined to one sample.
    fn locate(reference: &[f32], window: &[f32]) -> usize {
        let frames = window.len() / 2;
        let error = |start: usize, stride: usize| -> f32 {
            (0..frames)
                .step_by(stride)
                .map(|frame| {
                    (reference[(start + frame) * 2] - window[frame * 2]).abs()
                        + (reference[(start + frame) * 2 + 1] - window[frame * 2 + 1]).abs()
                })
                .sum()
        };
        let last = reference.len() / 2 - frames;
        let coarse = (0..last)
            .step_by(48)
            .min_by(|left, right| error(*left, 8).total_cmp(&error(*right, 8)))
            .unwrap();
        (coarse.saturating_sub(48)..(coarse + 48).min(last))
            .min_by(|left, right| error(*left, 1).total_cmp(&error(*right, 1)))
            .unwrap()
    }

    fn media_ms(packets: &[EncodedOpusPacket]) -> u64 {
        packets
            .iter()
            .map(|packet| u64::from(packet.media_samples_per_channel))
            .sum::<u64>()
            / 48
    }

    #[test]
    fn forwarded_packets_keep_flowing_while_preparing_and_the_swap_lands_on_the_target() {
        let server = RangelessServer::start(TRANSIENTS.to_vec());
        let reference = reference_pcm(TRANSIENTS);
        let source = source_packets(TRANSIENTS);
        assert!(source.len() > 200, "the fixture is long enough to seek in");
        let (sink, handle) = StepPacketSink::new();
        let session = Arc::new(PlaybackSession::new());
        session.configure_fallback_seeking(true);
        session
            .play(server.source(), OpusEncoderOutput::new(sink).unwrap())
            .unwrap();
        assert_eq!(
            session.processing_mode(),
            Some(PlaybackProcessingMode::Passthrough)
        );
        handle.release(120);
        assert!(handle.wait_for_calls(121));
        server.hold_new_connections();

        let seeking = Arc::clone(&session);
        let seek = thread::spawn(move || seeking.seek(Duration::from_millis(500)));
        assert!(wait_for_pending_seek(&session));
        let at_request = handle.snapshot().packets.len() + 1;
        handle.release(1);
        assert!(server.wait_for_connections(2));

        // Nothing is converted or decoded while the replacement is still being prepared.
        handle.release(40);
        assert!(handle.wait_for_calls(at_request + 40));
        assert_eq!(
            session.processing_mode(),
            Some(PlaybackProcessingMode::Passthrough)
        );
        let forwarded = handle.snapshot().packets;
        assert!(
            forwarded
                .iter()
                .skip(100)
                .all(|packet| source
                    .iter()
                    .any(|s| s.packet.data().unwrap() == packet.bytes)),
            "the old stream is forwarded unchanged until the replacement is ready"
        );

        server.release();
        let deadline = Instant::now() + TEST_TIMEOUT;
        while !seek.is_finished() {
            assert!(Instant::now() < deadline, "the seek never completed");
            handle.release(1);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(seek.join().unwrap().is_ok());
        assert_eq!(
            session.processing_mode(),
            Some(PlaybackProcessingMode::Transcoding)
        );
        handle.release(100_000);
        assert_eq!(session.wait(), PlaybackState::Completed);

        let packets = handle.snapshot().packets;
        // Everything up to the encoder's final flush follows on without a gap or an overlap,
        // across the conversion to PCM and the swap.
        assert_timeline_is_contiguous(&packets[..packets.len() - 3]);

        // The client decodes what the swap sent. Find where in the track it is, 600 ms after
        // the seam, and check it is the target plus the playback that happened meanwhile.
        let client = client_pcm(&packets);
        let swap_ms = media_ms(&packets[..(at_request + 40).min(packets.len())]);
        let window_start = (swap_ms as usize + 600) * 48;
        let window = &client[window_start * 2..(window_start + 2_400) * 2];
        let found_ms = locate(&reference, window) as i64 / 48 - 600;
        let elapsed_ms = media_ms(&packets[..(at_request + 40 + 10).min(packets.len())]) as i64
            - media_ms(&packets[..at_request]) as i64;
        assert!(
            (500..=500 + elapsed_ms + 400).contains(&found_ms),
            "the replacement is at {found_ms} ms; the target was 500 ms and {elapsed_ms} ms of \
             the old stream played while it was prepared"
        );
    }
}
