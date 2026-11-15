//! Playback lifecycle diagnostics reach the session's own observability context.
//!
//! This is a single-test binary on purpose. Diagnostics travel through `tracing` callsites whose
//! enablement is cached process-wide, and `tracing-core` computes that cache from whichever
//! thread reaches a callsite first when only one dispatcher exists. Another test thread running a
//! session with the default handle could therefore silence a callsite for this test's dispatcher.
//! Isolating the process keeps the assertion deterministic; session independence and
//! backpressure are verified separately from per-session state.

use std::time::Duration;

use remedia_core::{InputSource, NullOutput, PlaybackSession, PlaybackState};
use remedia_observability::{Observability, ObservabilityConfig};

fn wave(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut bytes = Vec::new();

    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&96_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }

    bytes
}

#[test]
fn starting_and_started_events_carry_one_playback_id_per_session() {
    let observability = Observability::new(
        ObservabilityConfig::default()
            .with_filter("info")
            .expect("valid filter")
            .with_console_enabled(false),
    )
    .expect("create observability context");
    let diagnostics = observability.diagnostics();
    let sessions = [
        PlaybackSession::with_observability(observability.handle()),
        PlaybackSession::with_observability(observability.handle()),
    ];

    for session in &sessions {
        session
            .play(InputSource::bytes(wave(&[2000; 4800])), NullOutput::new())
            .expect("start playback");
    }

    let mut starting = Vec::new();
    let mut started = Vec::new();
    while starting.len() < 2 || started.len() < 2 {
        let event = diagnostics
            .recv_timeout(Duration::from_secs(10))
            .expect("playback lifecycle diagnostics arrive");
        let id = event.playback_id.expect("playback ID field");

        match event.event.as_str() {
            "core.playback.starting" => starting.push(id),
            "core.playback.started" => started.push(id),
            _ => {}
        }
    }

    starting.sort_unstable();
    started.sort_unstable();
    assert_ne!(starting[0], starting[1], "sessions have distinct IDs");
    assert_eq!(starting, started, "each session reports both events");
    for session in &sessions {
        assert_eq!(session.wait(), PlaybackState::Completed);
    }
    assert_eq!(diagnostics.dropped_events(), 0);
}
