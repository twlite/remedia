use super::*;

#[test]
fn streams_irregular_chunks_and_clean_eof_into_ffmpeg_incrementally() {
    let bytes = pcm16_wave(48_000, &[vec![2048; 12_345]]);
    let (source, writer) = InputSource::byte_stream(257).unwrap();
    let producer = thread::spawn(move || {
        let mut offset = 0;
        for chunk_size in [1, 17, 3, 64, 9, 127, 5, 31] {
            if offset >= bytes.len() {
                break;
            }
            let end = (offset + chunk_size).min(bytes.len());
            writer.write(&bytes[offset..end]).unwrap();
            offset = end;
        }
        while offset < bytes.len() {
            let end = (offset + 23).min(bytes.len());
            writer.write(&bytes[offset..end]).unwrap();
            offset = end;
        }
        writer.finish().unwrap();
    });

    let mut decoder = AudioDecoder::open(source).unwrap();
    assert_eq!(decoder.info().channels, Some(1));
    assert_eq!(frame_count(&collect_frames(&mut decoder)), 12_345);
    assert!(decoder.next_frame().unwrap().is_none());
    producer.join().unwrap();
}

#[test]
fn producer_failure_during_probe_reaches_the_decoder_error() {
    let (source, writer) = InputSource::byte_stream(128).unwrap();
    let producer = thread::spawn(move || {
        writer.write(b"RIFF\0\0\0\0WAVEfmt ").unwrap();
        writer.fail("generated source failed").unwrap();
    });

    let error = AudioDecoder::open(source)
        .err()
        .expect("the producer failure must reject media probing");
    producer.join().unwrap();
    assert!(matches!(
        error,
        DecoderError::InputFailure(message) if message == "generated source failed"
    ));
}

#[test]
fn producer_failure_after_open_propagates_during_decoding() {
    let sample_count = 480_000;
    let bytes = pcm16_wave(48_000, &[vec![1024; sample_count]]);
    let (source, writer) = InputSource::byte_stream(bytes.len()).unwrap();
    let (continue_tx, continue_rx) = mpsc::channel();
    let producer = thread::spawn(move || {
        writer.write(&bytes).unwrap();
        continue_rx.recv().unwrap();
        writer.fail("producer disconnected").unwrap();
    });

    let mut decoder = AudioDecoder::open(source).unwrap();
    continue_tx.send(()).unwrap();
    let mut decoded = 0;
    let error = loop {
        match decoder.next_frame() {
            Ok(Some(frame)) => decoded += frame.samples_per_channel,
            Ok(None) => panic!("producer failure was lost at EOF"),
            Err(error) => break error,
        }
    };
    producer.join().unwrap();

    assert!(decoded > 0);
    assert!(matches!(
        error,
        DecoderError::InputFailure(message) if message == "producer disconnected"
    ));
}

#[test]
fn byte_stream_seeks_forward_and_rejects_backward_seek_without_losing_audio() {
    let bytes = pcm16_wave(48_000, &[vec![2048; 480]]);
    let (source, writer) = InputSource::byte_stream(4096).unwrap();
    writer.write(&bytes).unwrap();
    writer.finish().unwrap();
    let mut decoder = AudioDecoder::open(source).unwrap();

    decoder.seek(Duration::from_millis(1)).unwrap();
    assert!(matches!(
        decoder.seek(Duration::ZERO),
        Err(DecoderError::UnsupportedSeeking(_))
    ));
    let remaining = collect_frames(&mut decoder);
    assert_eq!(remaining[0].start_sample, 48);
    assert_eq!(frame_count(&remaining), 432);
}

#[test]
fn cancelling_a_blocked_custom_io_read_releases_decoder_and_writer() {
    let (source, writer) = InputSource::byte_stream(64).unwrap();
    let cancellation = CancellationToken::new();
    let decode_cancellation = cancellation.clone();
    let decoder = thread::spawn(move || {
        AudioDecoder::open_with_options(
            source,
            DecoderOptions {
                io_timeout: Duration::from_secs(10),
                cancellation: decode_cancellation,
            },
        )
    });

    let started = Instant::now();
    thread::sleep(Duration::from_millis(20));
    cancellation.cancel();
    assert!(matches!(
        decoder.join().unwrap(),
        Err(DecoderError::Cancelled)
    ));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        writer.write(b"after cancel"),
        Err(remedia_core::ByteStreamError::ConsumerClosed)
    );
}

#[test]
fn bounded_stream_decodes_generated_large_input_without_buffering_the_source() {
    let sample_count = 480_000_usize;
    let data_bytes = sample_count * 2;
    let header = pcm16_wave_header(48_000, 1, data_bytes);
    let capacity = 4096;
    let (source, writer) = InputSource::byte_stream(capacity).unwrap();
    let peak_pending = Arc::new(AtomicUsize::new(0));
    let producer_peak = Arc::clone(&peak_pending);
    let producer = thread::spawn(move || {
        writer.write(&header).unwrap();
        let samples_per_chunk = 1024;
        let mut remaining = sample_count;
        while remaining > 0 {
            let samples = remaining.min(samples_per_chunk);
            let chunk = vec![0x10_u8; samples * 2];
            writer.write(&chunk).unwrap();
            let pending = writer.pending_bytes();
            producer_peak.fetch_max(pending, Ordering::Relaxed);
            remaining -= samples;
        }
        writer.finish().unwrap();
        writer.capacity_bytes()
    });

    let mut decoder = AudioDecoder::open(source).unwrap();
    assert_eq!(frame_count(&collect_frames(&mut decoder)), sample_count);
    let producer_capacity = producer.join().unwrap();

    assert_eq!(producer_capacity, capacity);
    assert!(peak_pending.load(Ordering::Relaxed) <= capacity);
}
