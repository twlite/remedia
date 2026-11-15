//! Opus pre-skip against real FFmpeg/libopus streams.
//!
//! Every file here was written by FFmpeg, so it carries a genuine 312-sample pre-skip. The
//! reference is FFmpeg's own decode (pre-skip and end trimming applied). The "client" is a fresh
//! libopus decoder fed the packets Remedia outputs, as a Discord client would.

use super::*;
use crate::decoder::DemuxedAudioPacket;

pub(super) const ABRUPT: &[u8] =
    include_bytes!("../../../tests/fixtures/ffmpeg-opus-abrupt-start.ogg");
pub(super) const SILENT_LEAD: &[u8] =
    include_bytes!("../../../tests/fixtures/ffmpeg-opus-silent-lead.ogg");
pub(super) const SILENT_LEAD_WEBM: &[u8] =
    include_bytes!("../../../tests/fixtures/ffmpeg-opus-silent-lead.webm");
pub(super) const SILENT_LEAD_LONG: &[u8] =
    include_bytes!("../../../tests/fixtures/ffmpeg-opus-silent-lead-long.ogg");
pub(super) const TRANSIENTS: &[u8] =
    include_bytes!("../../../tests/fixtures/ffmpeg-opus-transients-long.ogg");

/// The pre-skip FFmpeg wrote for these files, in samples at 48 kHz.
pub(super) const PRE_SKIP: usize = 312;

/// Interleaved stereo PCM as FFmpeg decodes the file, with pre-skip and end trimming applied.
pub(super) fn reference_pcm(bytes: &[u8]) -> Vec<f32> {
    let mut decoder = AudioDecoder::open(InputSource::bytes(bytes.to_vec())).unwrap();
    let mut pcm = Vec::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        pcm.extend(frame.samples);
    }
    pcm
}

pub(super) fn source_packets(bytes: &[u8]) -> Vec<DemuxedAudioPacket> {
    let mut decoder = AudioDecoder::open_for_playback(
        InputSource::bytes(bytes.to_vec()),
        DecoderOptions::default(),
    )
    .unwrap();
    let mut packets = Vec::new();
    while let Some(packet) = decoder.read_compressed_packet().unwrap() {
        packets.push(packet);
    }
    packets
}

pub(super) fn payload(packet: &DemuxedAudioPacket) -> &[u8] {
    packet.packet.data().unwrap()
}

/// Everything the sink received when `bytes` plays to the end.
pub(super) fn play_to_end(bytes: &[u8]) -> Vec<EncodedOpusPacket> {
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
    handle.snapshot().packets
}

/// What a client sees: every packet decoded in order by one libopus decoder. Each packet
/// contributes its full encoded duration, including any leading pre-roll that is not media.
pub(super) fn client_pcm(packets: &[EncodedOpusPacket]) -> Vec<f32> {
    let mut decoder = opus2::Decoder::new(48_000, opus2::Channels::Stereo).unwrap();
    let mut pcm = Vec::new();
    for packet in packets {
        let mut block = vec![0.0; packet.samples_per_channel as usize * 2];
        let decoded = decoder
            .decode_float(&packet.bytes, &mut block, false)
            .unwrap();
        assert_eq!(decoded, packet.samples_per_channel as usize);
        pcm.extend(block);
    }
    pcm
}

pub(super) fn media_samples(packets: &[EncodedOpusPacket]) -> u64 {
    packets
        .iter()
        .map(|packet| u64::from(packet.media_samples_per_channel))
        .sum()
}

pub(super) fn assert_timeline_is_contiguous(packets: &[EncodedOpusPacket]) {
    assert_eq!(packets[0].start_sample, 0);
    assert!(
        packets.windows(2).all(|pair| pair[1].start_sample
            == pair[0].start_sample + u64::from(pair[0].media_samples_per_channel)),
        "the media timeline has no gap or overlap"
    );
}

/// Largest absolute difference between `client` (from `client_start`) and `reference` over the
/// media samples `[from, to)`.
fn max_error(
    client: &[f32],
    client_start: usize,
    reference: &[f32],
    from: usize,
    to: usize,
) -> f32 {
    (from * 2..to * 2)
        .map(|index| (client[client_start * 2 + index] - reference[index]).abs())
        .fold(0.0, f32::max)
}

#[test]
fn real_ffmpeg_opus_files_are_eligible_and_report_their_pre_skip() {
    for (name, bytes) in [
        ("abrupt ogg", ABRUPT),
        ("silent-lead ogg", SILENT_LEAD),
        ("silent-lead webm", SILENT_LEAD_WEBM),
    ] {
        let reader = AudioDecoder::open_for_playback(
            InputSource::bytes(bytes.to_vec()),
            DecoderOptions::default(),
        )
        .unwrap();
        assert_eq!(reader.opus_passthrough_rejection(), None, "{name}");
        assert_eq!(reader.opus_pre_skip() as usize, PRE_SKIP, "{name}");
    }
}

#[test]
fn an_encoder_delay_pre_roll_is_forwarded_with_the_first_packet_unchanged() {
    for (name, bytes) in [
        ("ogg", SILENT_LEAD),
        ("webm", SILENT_LEAD_WEBM),
        ("abrupt ogg", ABRUPT),
    ] {
        let source = source_packets(bytes);
        let packets = play_to_end(bytes);
        let reference = reference_pcm(bytes);

        assert_eq!(
            payload(&source[0]),
            packets[0].bytes,
            "{name}: packet 0 is untouched"
        );
        assert_eq!(packets[0].samples_per_channel, 960, "{name}");
        assert_eq!(
            packets[0].media_samples_per_channel as usize,
            960 - PRE_SKIP,
            "{name}: the pre-roll is accounted as non-media"
        );
        assert_timeline_is_contiguous(&packets);
        assert_eq!(
            media_samples(&packets) as usize,
            reference.len() / 2,
            "{name}"
        );

        // Every packet except the end-trimmed last one is the source's, byte for byte.
        let forwarded = packets.len() - 2;
        for index in 0..forwarded {
            assert_eq!(
                payload(&source[index]),
                packets[index].bytes,
                "{name} packet {index}"
            );
        }

        // Decoded by a fresh client, the audio is FFmpeg's decode delayed by exactly the pre-roll.
        let client = client_pcm(&packets);
        let clean = (forwarded - 1) * 960 - PRE_SKIP;
        assert!(
            // The packets are byte-identical; the bound covers FFmpeg's native Opus decoder
            // versus libopus rounding (about -55 dBFS here).
            max_error(&client, PRE_SKIP, &reference, 0, clean) < 2e-3,
            "{name}: client audio matches the reference"
        );
        // The pre-roll is what a player that ignored the pre-skip would play: silence, plus the
        // transform's pre-echo (about -30 dB below the audio that follows) for an abrupt start.
        let preroll = client[..PRE_SKIP * 2]
            .iter()
            .fold(0.0_f32, |peak, s| peak.max(s.abs()));
        let onset = client[PRE_SKIP * 2..(PRE_SKIP + 960) * 2]
            .iter()
            .fold(0.0_f32, |peak, s| peak.max(s.abs()));
        assert!(
            preroll <= 1e-3_f32.max(onset * 0.1),
            "{name}: pre-roll {preroll}, onset {onset}"
        );
    }
}

/// The longest run of consecutive source packets (from source index `from`) inside the output:
/// the output index it starts at, the source index of its first packet, and its length.
pub(super) fn longest_forwarded_run(
    packets: &[EncodedOpusPacket],
    source: &[DemuxedAudioPacket],
    from: usize,
) -> (usize, usize, usize) {
    let mut best = (0, 0, 0);
    for start in 0..packets.len() {
        let Some(k) = (from..source.len()).find(|k| payload(&source[*k]) == packets[start].bytes)
        else {
            continue;
        };
        let length = (0..)
            .take_while(|offset| {
                start + offset < packets.len()
                    && k + offset < source.len()
                    && payload(&source[k + offset]) == packets[start + offset].bytes
            })
            .count();
        if length > best.2 {
            best = (start, k, length);
        }
    }
    best
}

/// The run of source packets that ends the output: the index of its first packet and the source
/// index that packet is. The final source packet is end-trimmed and therefore re-encoded, so the
/// run ends one packet before the end of both lists.
pub(super) fn forwarded_run(
    packets: &[EncodedOpusPacket],
    source: &[DemuxedAudioPacket],
) -> (usize, usize) {
    let last = packets.len() - 2;
    let last_source = source.len() - 2;
    let mut start = last + 1;
    while start > 0
        && last - (start - 1) <= last_source
        && payload(&source[last_source - (last - (start - 1))]) == packets[start - 1].bytes
    {
        start -= 1;
    }
    (start, (last_source + start).saturating_sub(last))
}

#[test]
fn a_streamed_pre_skip_source_matches_the_buffered_one() {
    let (source, producer) = InputSource::byte_stream(1_024).unwrap();
    let bytes = SILENT_LEAD.to_vec();
    let feeder = thread::spawn(move || {
        for chunk in bytes.chunks(509) {
            producer.write(chunk).unwrap();
        }
        producer.finish().unwrap();
    });
    let (sink, handle) = StepPacketSink::new();
    handle.release(100_000);
    let session = PlaybackSession::new();
    session
        .play(source, OpusEncoderOutput::new(sink).unwrap())
        .unwrap();
    assert_eq!(session.wait(), PlaybackState::Completed);
    feeder.join().unwrap();
    assert_eq!(handle.snapshot().packets, play_to_end(SILENT_LEAD));
}

/// A fresh client decoder's residual against the reference over `blocks` consecutive 20 ms
/// blocks, starting at client index `client_start` and media sample `media_start`.
pub(super) fn residual_db(
    client: &[f32],
    client_start: usize,
    reference: &[f32],
    media_start: usize,
    blocks: usize,
) -> Vec<f32> {
    (0..blocks)
        .map(|block| {
            let (mut error, mut signal, mut heard) = (0.0_f32, 0.0_f32, 0.0_f32);
            for n in 0..960 {
                let c = client_start + block * 960 + n;
                let m = media_start + block * 960 + n;
                for channel in 0..2 {
                    let wanted = reference[m * 2 + channel];
                    error += (client[c * 2 + channel] - wanted).powi(2);
                    signal += wanted * wanted;
                    heard += client[c * 2 + channel].powi(2);
                }
            }
            if (signal / 1920.0).sqrt() < 0.004 {
                // A near-silent block has no meaningful relative error: it must stay near-silent.
                return if (heard / 1920.0).sqrt() < 0.01 {
                    -100.0
                } else {
                    0.0
                };
            }
            10.0 * (error / signal).log10()
        })
        .collect()
}

/// Seek `bytes` to `target` samples while playing, then collect everything the sink receives.
pub(super) fn packets_after_seek(bytes: &[u8], target: u64) -> Vec<EncodedOpusPacket> {
    let (sink, handle) = StepPacketSink::new();
    let session = Arc::new(PlaybackSession::new());
    session
        .play(
            InputSource::bytes(bytes.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert!(handle.wait_for_calls(1));
    let seeking_session = Arc::clone(&session);
    let position = Duration::from_nanos(target * 1_000_000_000 / 48_000);
    let (response_tx, response_rx) = mpsc::sync_channel(1);
    let seeker = thread::spawn(move || {
        let _ = response_tx.send(seeking_session.seek(position));
    });
    assert!(handle.wait_for_pending_seek(&session));
    handle.release(1);
    response_rx.recv_timeout(TEST_TIMEOUT).unwrap().unwrap();
    seeker.join().unwrap();
    handle.release(100_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    let packets = handle.snapshot().packets;
    // The first packet was sent before the seek.
    packets[1..].to_vec()
}

pub(super) fn assert_seek_is_seamless(target: u64) {
    assert_seek_is_seamless_in(SILENT_LEAD_LONG, target, [-15.0, -35.0, -12.0]);
}

/// Seek `bytes` to `target` and check the seam; `limits` bound the first block after the
/// junction, the eighth, and the last block before it, in dB relative to the reference.
pub(super) fn assert_seek_is_seamless_in(bytes: &[u8], target: u64, limits: [f32; 3]) {
    let reference = reference_pcm(bytes);
    let source = source_packets(bytes);
    let packets = packets_after_seek(bytes, target);
    let lead = (target % 960) as usize;

    assert_eq!(
        packets[0].start_sample, target,
        "the first packet starts at the target"
    );
    assert_eq!(packets[0].samples_per_channel, 960);
    assert_eq!(packets[0].media_samples_per_channel as usize, 960 - lead);
    assert!(
        packets.windows(2).all(|pair| pair[1].start_sample
            == pair[0].start_sample + u64::from(pair[0].media_samples_per_channel)),
        "no gap or overlap on the output timeline"
    );

    // The encoder runs for at least its warm-up before packets resume.
    let (junction, k) = forwarded_run(&packets, &source);
    assert!(
        junction >= 6,
        "re-encoded {junction} packets before resuming"
    );
    for (offset, packet) in packets[junction..packets.len() - 2].iter().enumerate() {
        assert_eq!(
            payload(&source[k + offset]),
            packet.bytes,
            "forwarded packet {offset}"
        );
    }

    // What a client hears across the seam. Media sample m plays at client index
    // (m - target) + lead + 312; the junction is where source packet k starts.
    let client = client_pcm(&packets);
    let junction_client: usize = packets[..junction]
        .iter()
        .map(|packet| packet.samples_per_channel as usize)
        .sum();
    let junction_media = 960 * k - PRE_SKIP;
    assert_eq!(
        junction_client,
        junction_media - target as usize + lead + PRE_SKIP
    );
    let after = residual_db(&client, junction_client, &reference, junction_media, 8);
    let before = residual_db(
        &client,
        junction_client - 4 * 960,
        &reference,
        junction_media - 4 * 960,
        4,
    );
    println!(
        "seek to {target}: junction at packet {junction}; residual dB before {before:?} after {after:?}"
    );
    assert!(
        after[0] < limits[0],
        "first block after the seam: {after:?}"
    );
    assert!(after[7] < limits[1], "the decoder has settled: {after:?}");
    // A freshly reset encoder codes its first frames poorly; the last block before the seam is
    // settled and must line up in time (a lag would show as a residual near 0 dB).
    assert!(
        before[3] < limits[2],
        "re-encoded audio is aligned: {before:?}"
    );
}

#[test]
fn seeking_a_real_opus_file_resumes_packets_with_a_clean_seam() {
    assert_seek_is_seamless(96_000);
}

#[test]
fn seeking_inside_a_packet_of_a_real_opus_file_resumes_packets_with_a_clean_seam() {
    // 100 440 = 104 * 960 + 600, and 2.0925 s is exact in nanoseconds.
    assert_seek_is_seamless(100_440);
}

#[test]
fn seeking_a_real_opus_file_back_to_its_start_resumes_packets() {
    let source = source_packets(SILENT_LEAD_LONG);
    let packets = packets_after_seek(SILENT_LEAD_LONG, 0);
    // The first 300 ms are digital silence, which re-encodes to the source's own packets, so
    // only the length of the forwarded run is meaningful here.
    let (junction, _) = forwarded_run(&packets, &source);
    assert!(
        packets.len() - junction > 100,
        "packets resume: run starts at {junction}"
    );
    assert_eq!(packets[0].start_sample, 0);
}

#[test]
fn a_stream_whose_pre_skip_differs_from_the_encoder_lookahead_keeps_decoding_after_a_seek() {
    // The synthetic fixture claims a pre-skip of 0, which cannot line up with the encoder's delay.
    let fixture = include_bytes!("../../../tests/fixtures/generated-opus-passthrough-long.ogg");
    let source = source_packets(fixture);
    let packets = packets_after_seek(fixture, 96_000);
    let forwarded = packets
        .iter()
        .filter(|packet| source.iter().any(|s| payload(s) == packet.bytes))
        .count();
    assert_eq!(
        forwarded, 0,
        "no packet is forwarded because the seam could not be made clean"
    );
}

pub(super) fn stage_identity(session: &PlaybackSession) -> super::super::StageIdentity {
    super::super::StageIdentity {
        expected_generation: session.source_generation(),
        entry_id: "next".into(),
        queue_revision: 1,
        source_revision: 1,
    }
}

/// Play `outgoing`, crossfade into `incoming`, and wait until the incoming source is staged.
pub(super) fn start_crossfade(
    outgoing: &[u8],
    incoming: &[u8],
) -> (PlaybackSession, StepPacketHandle) {
    let (sink, handle) = StepPacketSink::new();
    let session = PlaybackSession::new();
    session
        .configure_crossfade(Some(
            super::super::CrossfadeConfig::new(1000, 5000).unwrap(),
        ))
        .unwrap();
    session
        .play(
            InputSource::bytes(outgoing.to_vec()),
            OpusEncoderOutput::new(sink).unwrap(),
        )
        .unwrap();
    assert!(handle.wait_for_calls(1));
    session
        .stage_next(
            InputSource::bytes(incoming.to_vec()),
            stage_identity(&session),
        )
        .unwrap();
    let deadline = Instant::now() + TEST_TIMEOUT;
    while session.staged_source().unwrap().buffered_samples < 96_000 {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    (session, handle)
}

#[test]
fn a_promoted_real_opus_file_resumes_packets_with_a_clean_seam() {
    let reference = reference_pcm(SILENT_LEAD_LONG);
    let source = source_packets(SILENT_LEAD_LONG);
    let (session, handle) = start_crossfade(SILENT_LEAD, SILENT_LEAD_LONG);
    handle.release(100_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(session.source_generation(), 2);
    // The file's last packet carries end trimming, so playback ends in PCM; what matters is the
    // long run of its own packets before that.
    let packets = handle.snapshot().packets;
    assert_timeline_is_contiguous(&packets);

    // The tail is the incoming file's own packets, byte for byte, and it starts well after the
    // overlap: the incoming source was decoded for its fade and the staged audio that followed.
    // From a point after the overlap the output is the incoming file's own packets, byte for
    // byte: its decoder moved to the playback worker and nothing was decoded again.
    let (junction, k, length) = longest_forwarded_run(&packets, &source, 100);
    assert!(length > 100, "only {length} packets were forwarded");
    assert!(
        junction > 100,
        "the run starts after the overlap, at {junction}"
    );

    // A fresh client decoder hears the incoming audio continue across the seam without a hole:
    // source packet k plays at the client index where the re-encoded audio ends.
    let client = client_pcm(&packets);
    let junction_client: usize = packets[..junction]
        .iter()
        .map(|packet| packet.samples_per_channel as usize)
        .sum();
    let junction_media = 960 * k - PRE_SKIP;
    let before = residual_db(
        &client,
        junction_client - 4 * 960,
        &reference,
        junction_media - 4 * 960,
        4,
    );
    let after = residual_db(&client, junction_client, &reference, junction_media, 8);
    println!(
        "promotion: junction at packet {junction}; residual dB before {before:?} after {after:?}"
    );
    assert!(
        before[3] < -12.0,
        "re-encoded audio is aligned with the packets: {before:?}"
    );
    assert!(after[0] < -15.0, "first block after the seam: {after:?}");
    assert!(after[7] < -35.0, "the decoder has settled: {after:?}");
}

#[test]
fn a_promoted_pre_skip_mismatch_keeps_transcoding() {
    // Handoff is only seamless for streams whose pre-skip equals the encoder lookahead.
    let outgoing = include_bytes!("../../../tests/fixtures/generated-opus-passthrough.ogg");
    let incoming = include_bytes!("../../../tests/fixtures/generated-opus-passthrough-long.ogg");
    let source = source_packets(incoming);
    let (session, handle) = start_crossfade(outgoing, incoming);
    handle.release(100_000);
    assert_eq!(session.wait(), PlaybackState::Completed);
    assert_eq!(
        session.processing_mode(),
        Some(PlaybackProcessingMode::Transcoding)
    );
    let packets = handle.snapshot().packets;
    assert!(
        !packets
            .iter()
            .rev()
            .take(100)
            .any(|packet| source.iter().any(|s| payload(s) == packet.bytes))
    );
}

#[test]
fn stopping_after_a_promoted_decoder_handoff_releases_every_worker() {
    let (session, handle) = start_crossfade(SILENT_LEAD, SILENT_LEAD_LONG);
    // Let the overlap complete and the decoder move, then stop while a packet is blocked.
    handle.release(260);
    assert!(handle.wait_for_calls(261));
    session.stop().unwrap();
    assert_eq!(session.state(), PlaybackState::Stopped);
    assert!(session.staged_source().is_none());
    assert!(handle.snapshot().cancelled);
}
