use super::*;

#[test]
fn converts_sample_rate_and_duplicates_mono_to_interleaved_stereo() {
    let input = pcm16_wave(22_050, &[vec![8192; 2205]]);
    let file = TempFile::new("音声.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();

    assert_eq!(decoder.output_format(), PcmFormat::NORMALIZED);
    assert_eq!(decoder.info().sample_rate, Some(22_050));
    assert_eq!(decoder.info().channels, Some(1));
    assert_eq!(decoder.info().duration, Some(Duration::from_millis(100)));

    let frames = collect_frames(&mut decoder);
    assert!(!frames.is_empty());
    assert_eq!(frame_count(&frames), 4800);

    let mut mono_output = Vec::new();
    for frame in &frames {
        assert_eq!(frame.samples.len(), frame.samples_per_channel * 2);
        let (pairs, remainder) = frame.samples.as_chunks::<2>();
        assert!(remainder.is_empty());
        for pair in pairs {
            assert_eq!(pair[0], pair[1], "mono should feed both stereo channels");
            assert!(pair[0].abs() <= 1.0);
            mono_output.push(pair[0]);
        }
    }
    assert!(
        mono_output[100..mono_output.len() - 100]
            .iter()
            .any(|sample| sample.abs() > 0.1)
    );
}

#[test]
fn preserves_left_right_order_when_interleaving_stereo_samples() {
    let input = pcm16_wave(48_000, &[vec![8192; 960], vec![-4096; 960]]);
    let file = TempFile::new("stereo.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();
    let frames = collect_frames(&mut decoder);

    assert_eq!(frame_count(&frames), 960);
    for pair in frames
        .iter()
        .flat_map(|frame| frame.samples.as_chunks::<2>().0)
    {
        assert!(pair[0] > 0.0, "left channel should remain first");
        assert!(pair[1] < 0.0, "right channel should remain second");
        assert!((pair[0] - 0.25).abs() < 0.01);
        assert!((pair[1] + 0.125).abs() < 0.01);
    }
}

#[test]
fn clamps_float_source_samples_to_the_normalized_range() {
    let input = float_wave(48_000, &[vec![2.0; 128], vec![-2.0; 128]]);
    let file = TempFile::new("out-of-range.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();
    let frames = collect_frames(&mut decoder);
    let samples = frames
        .iter()
        .flat_map(|frame| frame.samples.iter().copied())
        .collect::<Vec<_>>();

    assert_eq!(frame_count(&frames), 128);
    assert!(samples.contains(&1.0));
    assert!(samples.contains(&-1.0));
    assert!(samples.iter().all(|sample| (-1.0..=1.0).contains(sample)));
}

#[test]
fn downmixes_multichannel_audio_using_its_channel_layout() {
    let mut channels = vec![vec![0; 480]; 6];
    channels[2].fill(4096); // Front Center in the 5.1 channel mask below.
    let input = pcm16_wave_extensible(48_000, &channels, 0x3f);
    let file = TempFile::new("5point1.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();

    assert_eq!(decoder.info().channels, Some(6));
    assert!(decoder.info().channel_layout.is_some());
    let frames = collect_frames(&mut decoder);
    assert_eq!(frame_count(&frames), 480);

    let pairs = frames
        .iter()
        .flat_map(|frame| frame.samples.as_chunks::<2>().0)
        .collect::<Vec<_>>();
    assert!(pairs.iter().any(|pair| pair[0] > 0.05 && pair[1] > 0.05));
    assert!(pairs.iter().all(|pair| (pair[0] - pair[1]).abs() < 0.001));
}

#[test]
fn reports_sample_positions_and_stable_end_of_stream() {
    let input = pcm16_wave(48_000, &[vec![2048; 1920]]);
    let file = TempFile::new("eos.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();

    let mut expected_position = 0;
    while let Some(frame) = decoder.next_frame().unwrap() {
        assert_eq!(frame.start_sample, expected_position);
        assert_eq!(frame.samples.len(), frame.samples_per_channel * 2);
        expected_position += frame.samples_per_channel as u64;
    }
    assert_eq!(expected_position, 1920);
    assert!(decoder.next_frame().unwrap().is_none());
}

#[test]
fn decodes_owned_buffer_input_and_seeks_by_byte_offset() {
    let input = pcm16_wave(48_000, &[vec![4096; 48_000]]);
    let mut decoder = AudioDecoder::open(InputSource::bytes(input)).unwrap();

    assert_eq!(decoder.info().codec, "pcm_s16le");
    assert_eq!(decoder.info().duration, Some(Duration::from_secs(1)));
    assert!(decoder.next_frame().unwrap().is_some());
    decoder.seek(Duration::from_millis(500)).unwrap();
    let frames = collect_frames(&mut decoder);

    assert_eq!(frames.first().unwrap().start_sample, 24_000);
    assert_eq!(frame_count(&frames), 24_000);
}

#[test]
fn decodes_compressed_media_from_an_owned_buffer() {
    let input = include_bytes!("../fixtures/generated-tone.mp3").to_vec();
    let mut decoder = AudioDecoder::open(InputSource::bytes(input)).unwrap();

    assert!(decoder.info().codec.starts_with("mp3"));
    assert_eq!(decoder.info().sample_rate, Some(22_050));
    assert_eq!(decoder.info().channels, Some(1));
    assert!(frame_count(&collect_frames(&mut decoder)) > 0);
}

#[test]
fn mp3_encoder_delay_keeps_media_duration_and_routes_skip_diagnostics() {
    let observability = Observability::new(
        ObservabilityConfig::default()
            .with_filter("remedia_core::ffmpeg=trace")
            .unwrap()
            .with_console_enabled(false),
    )
    .unwrap();
    let diagnostics = observability.diagnostics();
    let dispatch = observability.handle().dispatch().unwrap().clone();

    let (metadata_duration, decoded_samples) = tracing::dispatcher::with_default(&dispatch, || {
        let results = (0..5)
            .map(|_| {
                let mut decoder = AudioDecoder::open(InputSource::bytes(
                    include_bytes!("../fixtures/generated-tone.mp3").to_vec(),
                ))
                .unwrap();
                let duration = decoder.info().duration.unwrap();
                let samples = frame_count(&collect_frames(&mut decoder));
                assert!(samples > 0);
                (duration, samples)
            })
            .collect::<Vec<_>>();
        *results.last().expect("five MP3 decodes completed")
    });

    let decoded_duration = decoded_samples as f64 / 48_000.0;
    assert!(decoded_samples > 0);
    assert!(
        (decoded_duration - metadata_duration.as_secs_f64()).abs() < 0.1,
        "decoded {decoded_duration:.3}s, metadata {metadata_duration:?}"
    );

    let events = std::iter::from_fn(|| diagnostics.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        !events.iter().any(|event| {
            matches!(
                event.fields.get("ffmpeg_message"),
                Some(DiagnosticValue::String(message))
                    if message.contains("Could not update timestamps for skipped samples")
                        || message.contains("Could not update timestamps for discarded samples")
            )
        }),
        "FFmpeg reported a timestamp adjustment warning after packet time base setup: {events:?}"
    );
    assert!(
        events.len() < 128,
        "repeated MP3 diagnostics were not bounded"
    );
    assert_eq!(diagnostics.dropped_events(), 0);
}

#[test]
fn empty_and_truncated_buffers_fail_as_invalid_media() {
    assert!(matches!(
        AudioDecoder::open(InputSource::bytes(Vec::new())),
        Err(DecoderError::InvalidMedia(_)
            | DecoderError::UnsupportedMedia(_)
            | DecoderError::OpenInput(_))
    ));

    assert!(matches!(
        AudioDecoder::open(InputSource::bytes(b"RIFF\0\0\0\0WAVEfmt ".to_vec())),
        Err(DecoderError::InvalidMedia(_)
            | DecoderError::UnsupportedMedia(_)
            | DecoderError::OpenInput(_))
    ));
}

#[test]
fn seeks_forward_and_discards_audio_before_the_requested_position() {
    let input = pcm16_wave(48_000, &[vec![4096; 48_000]]);
    let file = TempFile::new("seek.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();

    assert!(decoder.next_frame().unwrap().is_some());
    decoder.seek(Duration::from_millis(500)).unwrap();
    let frames = collect_frames(&mut decoder);

    assert_eq!(frames.first().unwrap().start_sample, 24_000);
    assert_eq!(frame_count(&frames), 24_000);
}

#[test]
fn rejects_seek_positions_past_a_known_duration() {
    let input = pcm16_wave(48_000, &[vec![4096; 480]]);
    let file = TempFile::new("short-seek.wav", &input);
    let mut decoder = AudioDecoder::open(InputSource::file(&file.path)).unwrap();

    assert!(matches!(
        decoder.seek(Duration::from_millis(20)),
        Err(DecoderError::SeekOutOfRange { .. })
    ));
}

#[test]
fn unsupported_or_missing_files_return_errors_without_panicking() {
    let missing = std::env::temp_dir().join("remedia-does-not-exist-音声.wav");
    assert!(matches!(
        AudioDecoder::open(InputSource::file(missing)),
        Err(DecoderError::SourceNotFound)
    ));

    let invalid = TempFile::new("not-a-media-file.bin", b"not media");
    assert!(matches!(
        AudioDecoder::open(InputSource::file(invalid.path.clone())),
        Err(DecoderError::UnsupportedMedia(_) | DecoderError::InvalidMedia(_))
    ));
}

#[test]
fn video_only_inputs_report_no_audio_stream() {
    let y4m = b"YUV4MPEG2 W2 H2 F25:1 Ip A1:1 C420jpeg\nFRAME\n\x10\x10\x10\x10\x80\x80";
    let file = TempFile::new("video-only.y4m", y4m);

    assert!(matches!(
        AudioDecoder::open(InputSource::file(file.path.clone())),
        Err(DecoderError::NoAudioStream)
    ));
}

#[test]
fn concurrent_decoders_keep_their_state_independent() {
    let input = pcm16_wave(22_050, &[vec![4096; 1102]]);
    let file = TempFile::new("concurrent.wav", &input);
    let path = file.path.clone();
    let workers = (0..2)
        .map(|_| {
            let path = path.clone();
            thread::spawn(move || {
                let mut decoder = AudioDecoder::open(InputSource::file(path)).unwrap();
                frame_count(&collect_frames(&mut decoder))
            })
        })
        .collect::<Vec<_>>();

    let counts = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(counts, vec![2399, 2399]);
}

#[test]
fn standalone_probe_shares_metadata_without_pcm_initialization() {
    use remedia_core::{ProbeOptions, probe};
    let bytes = pcm16_wave(22_050, &[vec![8192; 2205]]);
    let file = TempFile::new("probe.wav", &bytes);
    let expected = AudioDecoder::open(InputSource::file(&file.path))
        .unwrap()
        .info()
        .clone();
    assert_eq!(
        probe(InputSource::file(&file.path), ProbeOptions::default()).unwrap(),
        expected
    );
    assert_eq!(
        probe(InputSource::bytes(bytes.clone()), ProbeOptions::default()).unwrap(),
        expected
    );
    let (source, writer) = InputSource::byte_stream(1024).unwrap();
    let producer = thread::spawn(move || {
        let _ = writer.write_owned(bytes);
        let _ = writer.finish();
    });
    let info = probe(source, ProbeOptions::default()).unwrap();
    assert_eq!(info.codec, expected.codec);
    assert!(!info.metadata.contains_key("title"));
    producer.join().unwrap();
    let (source, _writer) = InputSource::byte_stream(1024).unwrap();
    assert!(matches!(
        probe(
            source,
            ProbeOptions {
                timeout: Duration::from_millis(20),
                ..Default::default()
            }
        ),
        Err(DecoderError::InputTimeout)
    ));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        probe(
            InputSource::file(&file.path),
            ProbeOptions {
                cancellation,
                ..Default::default()
            }
        ),
        Err(DecoderError::Cancelled)
    ));
}
