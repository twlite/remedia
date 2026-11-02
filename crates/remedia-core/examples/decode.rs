use std::error::Error;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use remedia_core::{AudioDecoder, InputSource};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let kind = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(usage_error)?;
    let value = arguments.next().ok_or_else(usage_error)?;
    if arguments.next().is_some() {
        return Err(usage_error().into());
    }

    let source = match kind.as_str() {
        "file" => InputSource::file(PathBuf::from(value)),
        "url" => InputSource::http_url(value.into_string().map_err(|_| usage_error())?),
        _ => return Err(usage_error().into()),
    };

    let mut decoder = AudioDecoder::open(source)?;
    let info = decoder.info();
    println!("Codec: {}", info.codec);
    println!("Audio stream: {}", info.stream_index);
    println!("Source rate: {}", display_optional(info.sample_rate));
    println!("Source channels: {}", display_optional(info.channels));
    println!(
        "Source channel layout: {}",
        info.channel_layout.as_deref().unwrap_or("unknown")
    );
    println!(
        "Source duration: {}",
        info.duration
            .map(format_duration)
            .unwrap_or_else(|| "unknown".to_owned())
    );

    let output_format = decoder.output_format();
    let mut output_sample_frames = 0_u64;
    let mut interleaved_samples = 0_u64;
    while let Some(frame) = decoder.next_frame()? {
        output_sample_frames += frame.samples_per_channel as u64;
        interleaved_samples += frame.samples.len() as u64;
    }

    let decoded_duration = Duration::from_secs_f64(
        output_sample_frames as f64 / f64::from(output_format.sample_rate()),
    );
    println!("Output format: 48 kHz, stereo, interleaved f32");
    println!("Decoded sample frames per channel: {output_sample_frames}");
    println!("Decoded interleaved f32 values: {interleaved_samples}");
    println!("Decoded duration: {}", format_duration(decoded_duration));
    Ok(())
}

fn display_optional<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn format_duration(duration: Duration) -> String {
    format!("{:.3} s", duration.as_secs_f64())
}

fn usage_error() -> io::Error {
    let program: OsString = std::env::args_os()
        .next()
        .unwrap_or_else(|| "decode".into());
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "Usage: {} <file|url> <path-or-http(s)-url>",
            program.to_string_lossy()
        ),
    )
}
