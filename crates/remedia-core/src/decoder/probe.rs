use super::*;

/// Limits and cancellation for disposable metadata inspection.
#[derive(Clone, Debug)]
pub struct ProbeOptions {
    /// Total operation deadline; defaults to 15 seconds and must be positive.
    pub timeout: Duration,
    /// Cancels native I/O, including blocked bounded byte-stream reads.
    pub cancellation: CancellationToken,
}
impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(15),
            cancellation: CancellationToken::new(),
        }
    }
}

/// Inspect a disposable source without constructing a codec decoder or playback session.
/// Runs blocking native I/O; async callers must use a blocking worker. Inputs are closed
/// before return. Stream inputs may be partially or fully consumed and cannot be reused.
pub fn probe(source: InputSource, options: ProbeOptions) -> Result<AudioStreamInfo, DecoderError> {
    if options.timeout.is_zero() {
        return Err(DecoderError::InvalidOptions(
            "probe timeout must be positive",
        ));
    }
    if options.cancellation.is_cancelled() {
        return Err(DecoderError::Cancelled);
    }
    let (done, completion) = std::sync::mpsc::channel();
    let cancellation = options.cancellation.clone();
    let deadline = options.timeout;
    let timer = std::thread::Builder::new()
        .name("remedia-probe-deadline".into())
        .spawn(move || {
            if completion.recv_timeout(deadline) == Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            {
                cancellation.cancel();
                true
            } else {
                false
            }
        })
        .map_err(|error| DecoderError::OpenInput(error.to_string()))?;
    let result = input::open(&source, options.timeout, options.cancellation.clone())
        .and_then(|input| inspect_audio(&input));
    let _ = done.send(());
    let timed_out = timer.join().unwrap_or(false);
    if timed_out {
        Err(DecoderError::InputTimeout)
    } else {
        result
    }
}
