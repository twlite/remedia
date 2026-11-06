use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::*;
use crate::connection::TestVoiceConnection;
use remedia_core::{
    InputSource, OpusEncoderOutput, PlaybackProcessingMode, PlaybackSession, PlaybackState,
};

#[test]
fn cancelling_a_parked_output_releases_its_packet_waiter() {
    let router = DiscordOpusSenderRouter::new();
    let sink = Arc::new(router.sink());
    let waiting_sink = Arc::clone(&sink);
    let cancellation = CancellationToken::new();
    let (finished, result) = mpsc::channel();
    let worker = thread::spawn(move || {
        let packet = EncodedOpusPacket {
            bytes: vec![0xF8, 0xFF, 0xFE],
            samples_per_channel: 960,
            media_samples_per_channel: 960,
            start_sample: 0,
        };
        let outcome = waiting_sink.send_packet(packet, &cancellation);
        let _ = finished.send(outcome);
    });

    sink.cancel();
    assert!(matches!(
        result.recv_timeout(Duration::from_secs(1)),
        Ok(Err(OutputError::Cancelled))
    ));
    worker.join().expect("parked sender worker should exit");
}

#[test]
fn passthrough_capability_survives_a_detached_transport() {
    let router = DiscordOpusSenderRouter::new();
    let sink = router.sink();
    let packet = EncodedOpusPacket {
        bytes: vec![0xF8, 0xFF, 0xFE],
        samples_per_channel: 960,
        media_samples_per_channel: 960,
        start_sample: 0,
    };

    assert!(sink.supports_passthrough_packet(&packet));
    router.detach();
    assert!(sink.supports_passthrough_packet(&packet));
}

#[test]
fn detaching_for_a_new_attempt_clears_a_previous_terminal_failure() {
    let router = DiscordOpusSenderRouter::new();
    router.fail("previous recovery failed");

    router.detach();

    let state = lock(&router.inner.state);
    assert!(state.target.is_none());
    assert!(state.failure.is_none());
}

#[test]
fn three_completed_playbacks_get_fresh_senders_on_one_voice_connection() {
    let voice = TestVoiceConnection::new(42);
    let connection_id = voice.connection().voice_connection_id();
    let router = DiscordOpusSenderRouter::new();
    router.attach(voice.connection().opus_sender());
    let cancellation = CancellationToken::new();

    for start_sample in [0, 960, 1_920] {
        let sender = router.sink();
        sender
            .send_packet(packet(start_sample), &cancellation)
            .unwrap();
        sender.finish(&cancellation).unwrap();
        assert_eq!(voice.connection().state(), ConnectionState::Connected);
        assert_eq!(voice.connection().voice_connection_id(), connection_id);
    }

    let packets = voice.wait_for_packets(3);
    assert_eq!(packets.len(), 3);
    assert!(packets.windows(2).all(|pair| pair[0].0 < pair[1].0));
    assert_eq!(
        voice
            .controls()
            .iter()
            .filter(|control| **control == "finish")
            .count(),
        3
    );
    let state = lock(&router.inner.state);
    assert!(state.active_playback_generation.is_none());
    assert!(state.target.is_none());
    assert!(
        state.connection.is_some(),
        "natural completion keeps voice open"
    );
}

#[test]
fn stopping_or_skipping_old_playback_cannot_cancel_a_new_sender() {
    let voice = TestVoiceConnection::new(43);
    let router = DiscordOpusSenderRouter::new();
    router.attach(voice.connection().opus_sender());
    let cancellation = CancellationToken::new();
    let stopped = router.sink();
    stopped.send_packet(packet(0), &cancellation).unwrap();
    // The fake transport receives queued packets on a different thread.
    // Observe the old packet before cancellation so this test verifies
    // generation isolation rather than racing the mock receiver.
    voice.wait_for_packets(1);
    stopped.cancel();

    let skipped = router.sink();
    // A delayed cancellation from the old track must be scoped to its playback generation.
    stopped.cancel();
    skipped.send_packet(packet(960), &cancellation).unwrap();
    skipped.finish(&cancellation).unwrap();

    let packets = voice.wait_for_packets(2);
    assert_eq!(packets.len(), 2);
    assert_ne!(packets[0].0, packets[1].0);
    assert_eq!(voice.connection().state(), ConnectionState::Connected);
}

#[test]
fn active_output_waits_for_replacement_and_delivers_only_new_packets_to_it() {
    let old_voice = TestVoiceConnection::new(46);
    let replacement_voice = TestVoiceConnection::new(46);
    let router = DiscordOpusSenderRouter::new();
    router.attach(old_voice.connection().opus_sender());
    let sender = Arc::new(router.sink());
    let cancellation = CancellationToken::new();
    sender.send_packet(packet(0), &cancellation).unwrap();
    // The fake voice receiver runs on its own thread. Ensure it has
    // recorded the old packet before checking transport isolation.
    old_voice.wait_for_packets(1);
    let playback_generation = lock(&router.inner.state)
        .active_playback_generation
        .expect("playback should remain active during replacement");

    router.detach();
    let waiting_sender = Arc::clone(&sender);
    let waiting_cancellation = cancellation.clone();
    let (started, started_rx) = mpsc::channel();
    let (delivered, delivered_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        started.send(()).unwrap();
        let result = waiting_sender.send_packet(packet(960), &waiting_cancellation);
        delivered.send(result).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    assert_eq!(old_voice.packets().len(), 1);
    router.attach(replacement_voice.connection().opus_sender());
    delivered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("parked packet should be routed after replacement")
        .unwrap();
    worker.join().unwrap();
    sender.finish(&cancellation).unwrap();

    assert_eq!(old_voice.packets().len(), 1);
    let replacement_packets = replacement_voice.wait_for_packets(1);
    assert_eq!(replacement_packets.len(), 1);
    assert_eq!(
        replacement_packets[0].1.start_sample, 960,
        "the parked packet keeps its source position"
    );
    assert_eq!(
        lock(&router.inner.state).transport_state,
        RouterTransportState::Connected
    );
    assert_ne!(
        old_voice.connection().voice_connection_id(),
        replacement_voice.connection().voice_connection_id()
    );
    assert_eq!(playback_generation, 1);
}

#[test]
fn pause_and_resume_controls_survive_the_routed_sender() {
    let voice = TestVoiceConnection::new(44);
    let router = DiscordOpusSenderRouter::new();
    router.attach(voice.connection().opus_sender());
    let sender = router.sink();
    let cancellation = CancellationToken::new();

    sender.pause(&cancellation).unwrap();
    sender.resume(&cancellation).unwrap();
    sender.finish(&cancellation).unwrap();

    let controls = voice.controls();
    assert!(controls.contains(&"pause"));
    assert!(controls.contains(&"resume"));
    assert!(controls.contains(&"finish"));
}

#[test]
fn playback_session_can_transcode_and_passthrough_over_consecutive_router_sinks() {
    let voice = TestVoiceConnection::new(45);
    let connection_id = voice.connection().voice_connection_id();
    let router = DiscordOpusSenderRouter::new();
    router.attach(voice.connection().opus_sender());

    let passthrough = PlaybackSession::new();
    passthrough
        .play(
            InputSource::bytes(
                include_bytes!(
                    "../../../remedia-core/tests/fixtures/generated-opus-passthrough.ogg"
                )
                .to_vec(),
            ),
            OpusEncoderOutput::new(router.sink()).unwrap(),
        )
        .unwrap();
    assert_eq!(passthrough.wait(), PlaybackState::Completed);
    assert_eq!(
        passthrough.processing_mode(),
        Some(PlaybackProcessingMode::Passthrough)
    );

    let transcoding = PlaybackSession::new();
    transcoding
        .play(
            InputSource::bytes(pcm_wave(&vec![1_024; 4_800])),
            OpusEncoderOutput::new(router.sink()).unwrap(),
        )
        .unwrap();
    assert_eq!(transcoding.wait(), PlaybackState::Completed);
    assert_eq!(
        transcoding.processing_mode(),
        Some(PlaybackProcessingMode::Transcoding)
    );

    let third = PlaybackSession::new();
    third
        .play(
            InputSource::bytes(pcm_wave(&vec![512; 4_800])),
            OpusEncoderOutput::new(router.sink()).unwrap(),
        )
        .unwrap();
    assert_eq!(third.wait(), PlaybackState::Completed);
    assert_eq!(
        third.processing_mode(),
        Some(PlaybackProcessingMode::Transcoding)
    );

    // Completion drains each sender's packets before the next playback.
    let packets = voice.wait_for_packets(3);
    assert!(packets.len() > 2);
    let sender_generations = packets
        .iter()
        .map(|(generation, _)| *generation)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(sender_generations.len(), 3);
    assert_eq!(voice.connection().voice_connection_id(), connection_id);
    assert_eq!(voice.connection().state(), ConnectionState::Connected);
}

fn packet(start_sample: u64) -> EncodedOpusPacket {
    EncodedOpusPacket {
        bytes: vec![0xF8, 0xFF, 0xFE],
        samples_per_channel: 960,
        media_samples_per_channel: 960,
        start_sample,
    }
}

fn pcm_wave(samples: &[i16]) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut bytes = Vec::with_capacity(44 + data_len);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&96_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}
