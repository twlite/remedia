//! Best-effort bounded local tail probe. No network or one-shot input is reopened.
use super::*;
use crate::crossfade::scheduling::TailAnalysis;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) struct TailProbe {
    result: Arc<AtomicU64>,
    cancellation: CancellationToken,
    worker: Option<JoinHandle<()>>,
}

impl TailProbe {
    pub(super) fn start(source: InputSource, duration: u64, fade: u64) -> Self {
        let result = Arc::new(AtomicU64::new(0));
        let cancellation = CancellationToken::new();
        let target = Arc::clone(&result);
        let token = cancellation.clone();
        let worker = thread::Builder::new()
            .name("remedia-tail".into())
            .spawn(move || {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    if let Some(end) = analyze(source, duration, fade, token) {
                        target.store(end, Ordering::Release);
                    }
                }));
            })
            .ok();
        Self {
            result,
            cancellation,
            worker,
        }
    }

    pub(super) fn endpoint(&self) -> Option<u64> {
        match self.result.load(Ordering::Acquire) {
            0 => None,
            end => Some(end),
        }
    }
}

impl Drop for TailProbe {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn analyze(
    source: InputSource,
    duration: u64,
    fade: u64,
    cancellation: CancellationToken,
) -> Option<u64> {
    let start = duration.saturating_sub(fade + 15 * 48_000);
    let mut decoder = AudioDecoder::open_with_options(
        source,
        DecoderOptions {
            cancellation: cancellation.clone(),
            ..DecoderOptions::default()
        },
    )
    .ok()?;
    if decoder.info().duration.map(samples_from_duration) != Some(duration) {
        return None;
    }
    if start > 0 {
        decoder
            .seek(
                Duration::from_secs(start / 48_000)
                    + Duration::from_nanos((start % 48_000) * 1_000_000_000 / 48_000),
            )
            .ok()?;
    }
    let mut analysis = TailAnalysis::new();
    let mut count = 0_u64;
    while !cancellation.is_cancelled() {
        let Some(frame) = decoder.next_frame().ok()? else {
            // Duration errors and truncated/replaced files cannot advance scheduling.
            if count.abs_diff(duration - start) > 4_800 {
                return None;
            }
            return analysis
                .endpoint()
                .map(|end| start + end)
                .filter(|end| *end < duration);
        };
        count += frame.samples_per_channel as u64;
        if count > 51 * 48_000 {
            return None;
        }
        analysis.push(&frame.samples);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wave() -> InputSource {
        let frames = 10 * 48_000;
        let bytes = (frames * 2) as u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&48_000_u32.to_le_bytes());
        wav.extend_from_slice(&96_000_u32.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&bytes.to_le_bytes());
        for frame in 0..frames {
            wav.extend_from_slice(&(if frame < 6 * 48_000 { 8000_i16 } else { 0 }).to_le_bytes());
        }
        InputSource::bytes(wav)
    }

    #[test]
    fn native_probe_confirms_eof_and_rejects_failed_or_cancelled_analysis() {
        let end = analyze(wave(), 480_000, 48_000, CancellationToken::new()).unwrap();
        assert!((300_000..330_000).contains(&end));
        assert!(analyze(wave(), 960_000, 48_000, CancellationToken::new()).is_none());
        assert!(
            analyze(
                InputSource::bytes(vec![0; 32]),
                480_000,
                48_000,
                CancellationToken::new()
            )
            .is_none()
        );
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(analyze(wave(), 480_000, 48_000, cancelled).is_none());
    }
}
