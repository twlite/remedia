//! Benchmark and audio comparison for the equal-power and adaptive crossfade
//! curves. No decoder, network, or Discord connection is involved: the
//! signals come from the same deterministic generators the unit tests use.
//!
//! ```text
//! cargo run --release -p remedia-core --example crossfade_adaptive -- bench
//! cargo run --release -p remedia-core --example crossfade_adaptive -- render <output-dir>
//! ```
//!
//! `bench` measures mixing cost per second of audio and counts heap
//! allocations made while mixing. `render` writes a 16-bit stereo WAV per
//! scenario and curve (two seconds of the outgoing track, the fade, two
//! seconds of the incoming track), plus `summary.md` with loudness metrics.

#[path = "../src/crossfade/fixtures.rs"]
mod fixtures;

use std::alloc::{GlobalAlloc, Layout, System};
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use fixtures::{
    SAMPLE_RATE, TrackSpec, loudness, max_step, music, noise_floor, peak, rms_dbfs, silence,
    trajectory_error, with_clicks,
};
use remedia_core::{CrossfadeCurve, CrossfadeWindow, MixStats, PcmCrossfade};

/// Counts every heap allocation so mixing can be shown not to make any.
struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);

        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract, which is
        // forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` and `layout` came from `alloc` above, which
        // delegated to `System`.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const FADE_MS: u32 = 5_000;
const BLOCK_FRAMES: usize = 960;
const LEAD_FRAMES: usize = 2 * SAMPLE_RATE;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);

    match arguments.next().as_deref() {
        Some("bench") => bench(),
        Some("render") => {
            let directory = arguments
                .next()
                .ok_or("render requires an output directory")?;

            render(Path::new(&directory))
        }
        _ => Err("usage: crossfade_adaptive bench | render <output-dir>".into()),
    }
}

/// One transition: what the outgoing and incoming tracks sound like.
struct Scenario {
    name: &'static str,
    description: &'static str,
    outgoing: Vec<f32>,
    incoming: Vec<f32>,
}

fn scenarios() -> Vec<Scenario> {
    // Each track is long enough for the lead-in or tail plus the whole fade.
    let frames = LEAD_FRAMES + FADE_MS as usize * SAMPLE_RATE / 1_000;
    let track = |seed, rms, bpm, root| music(TrackSpec::new(seed, rms, bpm, root), frames);
    // The incoming track starts where the outgoing overlap begins, so the
    // overlapped audio matches sample for sample.
    let identical = track(5, -12.0, 120.0, 130.81);
    let fade = window().samples();
    let clicks = [fade / 2, fade / 2 + 30_000, fade / 2 + 60_000];
    let outgoing_clicks = clicks.map(|click| LEAD_FRAMES + click);

    vec![
        Scenario {
            name: "quiet-into-loud",
            description: "quiet outro (-32 dBFS RMS) into a loud intro (-10 dBFS RMS)",
            outgoing: track(11, -32.0, 96.0, 110.0),
            incoming: track(29, -10.0, 128.0, 146.83),
        },
        Scenario {
            name: "loud-into-quiet",
            description: "loud outro (-10 dBFS RMS) into a quiet intro (-32 dBFS RMS)",
            outgoing: track(11, -10.0, 96.0, 110.0),
            incoming: track(29, -32.0, 128.0, 146.83),
        },
        Scenario {
            name: "loud-into-loud",
            description: "two loud masters (-9 dBFS RMS, peaks at 0.98)",
            outgoing: track(11, -9.0, 96.0, 110.0),
            incoming: track(29, -9.0, 128.0, 146.83),
        },
        Scenario {
            name: "identical-tracks",
            description: "the same loud track overlapped with itself, sample-aligned (fully correlated)",
            outgoing: identical.clone(),
            incoming: [&identical[LEAD_FRAMES * 2..], &identical[..LEAD_FRAMES * 2]].concat(),
        },
        Scenario {
            name: "silence-into-music",
            description: "digital silence into a loud intro",
            outgoing: silence(frames),
            incoming: track(29, -12.0, 128.0, 146.83),
        },
        Scenario {
            name: "noise-floor",
            description: "two -90 dBFS dither tails",
            outgoing: noise_floor(3, -90.0, frames),
            incoming: noise_floor(4, -90.0, frames),
        },
        Scenario {
            name: "sudden-peaks",
            description: "matched tracks with coincident full-scale clicks mid-fade (1.41x full scale)",
            outgoing: with_clicks(track(71, -22.0, 100.0, 110.0), &outgoing_clicks, 1.0),
            incoming: with_clicks(track(72, -22.0, 120.0, 123.47), &clicks, 1.0),
        },
    ]
}

fn window() -> CrossfadeWindow {
    CrossfadeWindow::from_millis(FADE_MS).expect("a valid fade duration")
}

/// Mix the overlap of a scenario and return the mixed overlap and its stats.
fn mix_overlap(
    curve: CrossfadeCurve,
    scenario: &Scenario,
) -> Result<(Vec<f32>, MixStats), Box<dyn Error>> {
    let frames = window().samples();
    let outgoing = &scenario.outgoing[LEAD_FRAMES * 2..(LEAD_FRAMES + frames) * 2];
    let incoming = &scenario.incoming[..frames * 2];
    let mut fade = PcmCrossfade::with_curve(window(), curve);
    let mut mixed = outgoing.to_vec();

    for (output, input) in mixed
        .chunks_mut(BLOCK_FRAMES * 2)
        .zip(incoming.chunks(BLOCK_FRAMES * 2))
    {
        fade.mix(output, input)
            .map_err(|error| format!("{error:?}"))?;
    }

    Ok((mixed, fade.stats()))
}

fn render(directory: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(directory)?;

    let mut summary = String::new();

    writeln!(
        summary,
        "# Adaptive crossfade comparison\n\n\
         Fade {FADE_MS} ms, 48 kHz stereo, {BLOCK_FRAMES}-frame blocks. Loudness is the RMS of\n\
         400 ms windows every 100 ms. \"Glide error\" is the mean distance between the output\n\
         loudness and a smoothstep glide from the outgoing level to the incoming level.\n"
    )?;
    writeln!(
        summary,
        "| Scenario | Curve | Overlap RMS | Peak | Largest 100 ms loudness step | Glide error | Deepest adaptive cut | Frames limited | Deepest limiting |\n\
         |---|---|---:|---:|---:|---:|---:|---:|---:|"
    )?;

    let fade_frames = window().samples();

    for scenario in scenarios() {
        for curve in CrossfadeCurve::ALL {
            let (mixed, stats) = mix_overlap(curve, &scenario)?;
            let outgoing = &scenario.outgoing[LEAD_FRAMES * 2..(LEAD_FRAMES + fade_frames) * 2];
            let incoming = &scenario.incoming[..fade_frames * 2];
            let series = loudness(&mixed);
            let mut timeline = scenario.outgoing[..LEAD_FRAMES * 2].to_vec();

            timeline.extend_from_slice(&mixed);
            timeline.extend_from_slice(
                &scenario.incoming[fade_frames * 2..(fade_frames + LEAD_FRAMES) * 2],
            );

            fs::write(
                directory.join(format!("{}-{}.wav", scenario.name, curve.as_str())),
                wav(&timeline),
            )?;

            // A glide from digital silence is meaningless, so it is not scored.
            let glide = if rms_dbfs(outgoing).min(rms_dbfs(incoming)) < -80.0 {
                "n/a".to_owned()
            } else {
                format!("{:.2} dB", trajectory_error(&mixed, outgoing, incoming))
            };

            writeln!(
                summary,
                "| {} | {} | {:.1} dBFS | {:.3} | {:.2} dB | {} | {:.2} dB | {} | {:.2} dB |",
                scenario.name,
                curve.as_str(),
                rms_dbfs(&mixed),
                peak(&mixed),
                max_step(&series),
                glide,
                stats.deepest_adaptive_gain_db,
                stats.limited_frames,
                stats.peak_reduction_db,
            )?;
        }
    }

    writeln!(summary, "\n## Scenarios\n")?;

    for scenario in scenarios() {
        writeln!(summary, "- `{}`: {}", scenario.name, scenario.description)?;
    }

    fs::write(directory.join("summary.md"), &summary)?;
    print!("{summary}");

    Ok(())
}

/// Encode interleaved stereo f32 as a 16-bit PCM WAV file.
fn wav(samples: &[f32]) -> Vec<u8> {
    let data_bytes = u32::try_from(samples.len() * 2).expect("WAV data fits in 32 bits");
    let mut bytes = Vec::with_capacity(44 + data_bytes as usize);

    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&(SAMPLE_RATE as u32).to_le_bytes());
    bytes.extend_from_slice(&(SAMPLE_RATE as u32 * 4).to_le_bytes());
    bytes.extend_from_slice(&4_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());

    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16;

        bytes.extend_from_slice(&value.to_le_bytes());
    }

    bytes
}

/// One timing row: nanoseconds per stereo frame and allocations per run.
struct Measurement {
    label: String,
    nanoseconds_per_frame: f64,
    allocations: u64,
    allocated_bytes: u64,
}

fn bench() -> Result<(), Box<dyn Error>> {
    const REPEATS: usize = 9;
    const PASSES: usize = 6;

    let frames = window().samples();
    let loud = scenarios()
        .into_iter()
        .find(|scenario| scenario.name == "loud-into-loud")
        .ok_or("missing scenario")?;
    let mismatched = scenarios()
        .into_iter()
        .find(|scenario| scenario.name == "quiet-into-loud")
        .ok_or("missing scenario")?;
    let mut rows = Vec::new();

    for (label, scenario) in [
        ("quiet-into-loud (limiter idle)", &mismatched),
        ("loud-into-loud (limiter busy)", &loud),
    ] {
        let outgoing = &scenario.outgoing[LEAD_FRAMES * 2..(LEAD_FRAMES + frames) * 2];
        let incoming = &scenario.incoming[..frames * 2];
        let mut work = outgoing.to_vec();

        // Copy-only baseline: the cost of refreshing the in-place buffer.
        rows.push(measure(
            &format!("{label}: copy baseline"),
            REPEATS,
            PASSES,
            frames,
            || {
                for (output, source) in work
                    .chunks_mut(BLOCK_FRAMES * 2)
                    .zip(outgoing.chunks(BLOCK_FRAMES * 2))
                {
                    output.copy_from_slice(source);
                }

                black_box(&work);
            },
        ));

        for curve in CrossfadeCurve::ALL {
            rows.push(measure(
                &format!("{label}: {}", curve.as_str()),
                REPEATS,
                PASSES,
                frames,
                || {
                    let mut fade = PcmCrossfade::with_curve(window(), curve);

                    for (output, (source, input)) in work.chunks_mut(BLOCK_FRAMES * 2).zip(
                        outgoing
                            .chunks(BLOCK_FRAMES * 2)
                            .zip(incoming.chunks(BLOCK_FRAMES * 2)),
                    ) {
                        output.copy_from_slice(source);
                        black_box(fade.mix(output, input)).expect("block fits the window");
                    }

                    black_box(&work);
                },
            ));
        }
    }

    println!("# Crossfade mixing cost\n");
    println!(
        "Median of {REPEATS} repeats, each mixing {PASSES} x {FADE_MS} ms of 48 kHz stereo in {BLOCK_FRAMES}-frame blocks.\n"
    );
    println!("| Case | ns / frame | Real-time CPU (1 stream) | Allocations / run | Bytes / run |");
    println!("|---|---:|---:|---:|---:|");

    for row in &rows {
        // One stereo frame must be produced every 1/48000 s.
        let budget = 1.0e9 / SAMPLE_RATE as f64;

        println!(
            "| {} | {:.1} | {:.3}% | {} | {} |",
            row.label,
            row.nanoseconds_per_frame,
            row.nanoseconds_per_frame / budget * 100.0,
            row.allocations,
            row.allocated_bytes,
        );
    }

    Ok(())
}

/// Time `work` and count its allocations. `work` performs `PASSES` overlaps.
fn measure(
    label: &str,
    repeats: usize,
    passes: usize,
    frames: usize,
    mut work: impl FnMut(),
) -> Measurement {
    // Warm caches and code before timing.
    work();

    let mut times = Vec::with_capacity(repeats);
    let mut allocations = 0;
    let mut allocated_bytes = 0;

    for _ in 0..repeats {
        let allocated_before = ALLOCATIONS.load(Ordering::Relaxed);
        let bytes_before = ALLOCATED_BYTES.load(Ordering::Relaxed);
        let started = Instant::now();

        for _ in 0..passes {
            work();
        }

        times.push(started.elapsed().as_secs_f64());
        allocations = (ALLOCATIONS.load(Ordering::Relaxed) - allocated_before) / passes as u64;
        allocated_bytes = (ALLOCATED_BYTES.load(Ordering::Relaxed) - bytes_before) / passes as u64;
    }

    times.sort_by(f64::total_cmp);

    Measurement {
        label: label.to_owned(),
        nanoseconds_per_frame: times[times.len() / 2] * 1.0e9 / (passes * frames) as f64,
        allocations,
        allocated_bytes,
    }
}
