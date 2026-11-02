use std::error::Error;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use remedia_core::CancellationToken;
use remedia_core::{
    AudioStreamInfo, InputSource, MediaOutput, NullOutput, NullOutputStatsHandle, OutputError,
    PcmFrame, PlaybackError, PlaybackSession, PlaybackState,
};
use remedia_observability::Observability;

fn main() -> Result<(), Box<dyn Error>> {
    let source = parse_source()?;
    let mut observability = Observability::from_env()?;
    let session = PlaybackSession::with_observability(observability.handle());
    let (output, stats) = PacedNullOutput::new(Duration::from_millis(50));
    let info = session.play(source, output)?;
    print_info(&info);
    println!("State after startup: {}", session.state());
    print_position(&session);

    match session.pause() {
        Ok(()) => {
            println!("Paused at a frame boundary");
            print_position(&session);
            let target = session.duration().unwrap_or_default() / 2;
            match session.seek(target) {
                Ok(()) => println!("Seeked to {target:.3?}"),
                Err(PlaybackError::UnsupportedSeeking(message)) => {
                    println!("This source cannot seek: {message}");
                }
                Err(error) => return Err(error.into()),
            }
            session.resume()?;
            println!("Resumed: {}", session.state());
        }
        Err(PlaybackError::InvalidState {
            state: PlaybackState::Completed,
            ..
        }) => println!("The source completed before the pause request."),
        Err(error) => return Err(error.into()),
    }

    match session.wait() {
        PlaybackState::Completed => println!("Playback completed naturally."),
        PlaybackState::Failed => {
            return Err(session
                .last_error()
                .unwrap_or(PlaybackError::WorkerPanicked)
                .into());
        }
        state => println!("Playback ended in state {state}."),
    }

    print_position(&session);
    let output_stats = stats.snapshot();
    println!("Frames consumed: {}", output_stats.frames_received);
    println!("Interleaved f32 values: {}", output_stats.samples_received);
    println!("Audio duration consumed: {:?}", output_stats.duration);
    println!(
        "Output finished successfully: {}",
        output_stats.finished_successfully
    );

    session.stop()?;
    println!("Session stopped and its worker/resources were released.");
    observability.force_flush()?;
    observability.shutdown()?;
    Ok(())
}

fn parse_source() -> Result<InputSource, Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let kind = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(usage_error)?;
    let value = arguments.next().ok_or_else(usage_error)?;
    if arguments.next().is_some() {
        return Err(usage_error().into());
    }

    match kind.as_str() {
        "file" => Ok(InputSource::file(PathBuf::from(value))),
        "url" => Ok(InputSource::http_url(
            value.into_string().map_err(|_| usage_error())?,
        )),
        _ => Err(usage_error().into()),
    }
}

fn print_info(info: &AudioStreamInfo) {
    println!("Codec: {}", info.codec);
    println!("Audio stream: {}", info.stream_index);
    println!("Source rate: {}", display_optional(info.sample_rate));
    println!("Source channels: {}", display_optional(info.channels));
    println!("Duration: {}", display_duration(info.duration));
}

fn print_position(session: &PlaybackSession) {
    let position = session.position();
    println!("Submitted position: {:?}", position.submitted);
    println!("Played position: {}", display_duration(position.played));
}

fn display_optional<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn display_duration(value: Option<Duration>) -> String {
    value
        .map(|duration| format!("{:.3} s", duration.as_secs_f64()))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn usage_error() -> io::Error {
    let program = std::env::args_os()
        .next()
        .unwrap_or_else(|| OsString::from("playback"));
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "Usage: {} <file|url> <path-or-http(s)-url>",
            program.to_string_lossy()
        ),
    )
}

/// Example-only pacing keeps the playback controls observable with short files.
struct PacedNullOutput {
    inner: NullOutput,
    minimum_frame_duration: Duration,
}

impl PacedNullOutput {
    fn new(minimum_frame_duration: Duration) -> (Self, NullOutputStatsHandle) {
        let inner = NullOutput::new();
        let stats = inner.stats_handle();
        (
            Self {
                inner,
                minimum_frame_duration,
            },
            stats,
        )
    }
}

impl MediaOutput for PacedNullOutput {
    fn write_frame(
        &self,
        frame: PcmFrame,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        let decoded_duration =
            Duration::from_secs_f64(frame.samples_per_channel as f64 / f64::from(48_000_u32));
        let wait_for = decoded_duration.max(self.minimum_frame_duration);
        let deadline = Instant::now() + wait_for;
        loop {
            if cancellation.is_cancelled() {
                return Err(OutputError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining.min(Duration::from_millis(5)));
        }
        self.inner.write_frame(frame, cancellation)
    }

    fn flush(
        &self,
        position: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), OutputError> {
        self.inner.flush(position, cancellation)
    }

    fn finish(&self, cancellation: &CancellationToken) -> Result<(), OutputError> {
        self.inner.finish(cancellation)
    }

    fn cancel(&self) {
        self.inner.cancel();
    }

    fn played_position(&self) -> Option<Duration> {
        self.inner.played_position()
    }
}
