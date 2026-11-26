//! Generate deterministic compressed fixtures through native FFmpeg, without media subprocesses.
//! Usage: `cargo run --release -p remedia-core --example resource_audit_fixtures -- DIRECTORY`.

use std::error::Error;
use std::path::Path;

use ffmpeg_next::{ChannelLayout, Packet, codec, encoder, format, frame};

const RATE: usize = 48_000;

fn signal(sample: usize, channel: usize) -> f32 {
    let t = sample as f64 / RATE as f64;
    let root = if channel == 0 { 220.0 } else { 277.0 };
    let phase = std::f64::consts::TAU;

    (0.18 * (phase * root * t + (phase * 0.7 * t).sin()).sin()
        + 0.12 * (phase * root * 1.5 * t).sin()
        + 0.07 * (phase * 3_100.0 * t).sin() * (phase * 1.3 * t).sin()) as f32
}

fn drain_packets(
    encoder: &mut codec::encoder::Audio,
    output: &mut format::context::Output,
) -> Result<(), ffmpeg_next::Error> {
    let mut packet = Packet::empty();

    loop {
        match encoder.receive_packet(&mut packet) {
            Ok(()) => {
                packet.set_stream(0);
                packet.rescale_ts(encoder.time_base(), output.stream(0).unwrap().time_base());
                packet.write_interleaved(output)?;
            }
            Err(ffmpeg_next::Error::Eof) => return Ok(()),
            Err(ffmpeg_next::Error::Other { errno }) if errno == ffmpeg_next::error::EAGAIN => {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
}

fn generate(path: &Path, codec_name: &str, seconds: usize) -> Result<(), Box<dyn Error>> {
    let codec = encoder::find_by_name(codec_name).ok_or_else(|| {
        std::io::Error::other(format!("native encoder {codec_name} is unavailable"))
    })?;
    let sample_format = codec
        .audio()?
        .formats()
        .into_iter()
        .flatten()
        .find(|format| matches!(format, format::Sample::F32(_)))
        .ok_or_else(|| std::io::Error::other("encoder does not accept float PCM"))?;
    let mut output = format::output(path)?;
    let mut encoder = codec::context::Context::new_with_codec(codec)
        .encoder()
        .audio()?;
    encoder.set_rate(RATE as i32);
    encoder.set_channel_layout(ChannelLayout::STEREO);
    encoder.set_format(sample_format);
    encoder.set_bit_rate(128_000);
    encoder.set_time_base((1, RATE as i32));

    if output
        .format()
        .flags()
        .contains(format::flag::Flags::GLOBAL_HEADER)
    {
        encoder.set_flags(codec::flag::Flags::GLOBAL_HEADER);
    }

    let mut encoder = encoder.open_as(codec)?;
    {
        let mut stream = output.add_stream(codec)?;
        stream.set_time_base((1, RATE as i32));
        stream.set_parameters(&encoder);
    }
    output.write_header()?;

    let frame_size = encoder.frame_size() as usize;
    if frame_size == 0 {
        return Err(std::io::Error::other("expected a fixed-size audio encoder").into());
    }

    let total_samples = seconds * RATE;
    let mut position = 0;

    while position < total_samples {
        let samples = frame_size.min(total_samples - position);
        let mut frame = frame::Audio::new(sample_format, samples, ChannelLayout::STEREO);
        frame.set_rate(RATE as u32);
        frame.set_pts(Some(position as i64));

        if sample_format.is_planar() {
            for channel in 0..2 {
                for (index, sample) in frame.plane_mut::<f32>(channel).iter_mut().enumerate() {
                    *sample = signal(position + index, channel);
                }
            }
        } else {
            for (index, sample) in frame.plane_mut::<f32>(0).iter_mut().enumerate() {
                *sample = signal(position + index / 2, index % 2);
            }
        }

        encoder.send_frame(&frame)?;
        drain_packets(&mut encoder, &mut output)?;
        position += samples;
    }

    encoder.send_eof()?;
    drain_packets(&mut encoder, &mut output)?;
    output.write_trailer()?;
    println!(
        "{}: {codec_name}, {total_samples} source samples/channel",
        path.display()
    );

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let directory = std::env::args_os()
        .nth(1)
        .ok_or_else(|| std::io::Error::other("expected an output directory"))?;
    let directory = Path::new(&directory);
    std::fs::create_dir_all(directory)?;
    ffmpeg_next::init()?;

    for (filename, codec, seconds) in [
        ("music60.opus", "libopus", 60),
        ("music15.opus", "libopus", 15),
        ("music3.opus", "libopus", 3),
        ("music60.mp3", "libmp3lame", 60),
        ("music3.mp3", "libmp3lame", 3),
        ("music3.aac", "aac", 3),
        ("music60.m4a", "aac", 60),
    ] {
        generate(&directory.join(filename), codec, seconds)?;
    }

    println!("Linked FFmpeg: {:?}", remedia_core::ffmpeg_build_info());

    Ok(())
}
