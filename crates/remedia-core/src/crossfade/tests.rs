//! Deterministic transition tests for the crossfade mixer.
//!
//! Every fixture comes from [`super::fixtures`], so each result is reproducible
//! from its parameters alone. Metrics compare the adaptive curve with the
//! equal-power curve on identical audio.

use super::curves::{MAX_CUT_DB, equal_power_gains};
use super::fixtures::{
    SAMPLE_RATE, TrackSpec, loudness, max_step, music, noise_floor, peak, rms_dbfs, silence,
    stereo_correlation, trajectory_error, with_clicks,
};
use super::*;

const WINDOW_MS: u32 = 4_000;

fn window() -> CrossfadeWindow {
    CrossfadeWindow::from_millis(WINDOW_MS).unwrap()
}

/// Two unrelated tracks, as the overlapping tail of one and head of the next.
fn unrelated(outgoing_rms: f64, incoming_rms: f64) -> (Vec<f32>, Vec<f32>) {
    let frames = window().samples();

    (
        music(TrackSpec::new(11, outgoing_rms, 96.0, 110.0), frames),
        music(TrackSpec::new(29, incoming_rms, 128.0, 146.83), frames),
    )
}

/// Mix `outgoing` and `incoming`, submitting block sizes from `sizes` in turn.
fn mix_chunked(
    curve: CrossfadeCurve,
    outgoing: &[f32],
    incoming: &[f32],
    sizes: &[usize],
) -> (Vec<f32>, PcmCrossfade) {
    let mut fade = PcmCrossfade::with_curve(window(), curve);
    let frames = outgoing.len() / 2;
    let mut mixed = outgoing.to_vec();
    let mut at = 0;
    let mut turn = 0;

    while at < frames {
        let size = sizes[turn % sizes.len()].min(frames - at);

        fade.mix(
            &mut mixed[at * 2..(at + size) * 2],
            &incoming[at * 2..(at + size) * 2],
        )
        .unwrap();

        at += size;
        turn += 1;
    }

    assert!(fade.complete());

    (mixed, fade)
}

fn mix(curve: CrossfadeCurve, outgoing: &[f32], incoming: &[f32]) -> (Vec<f32>, PcmCrossfade) {
    mix_chunked(curve, outgoing, incoming, &[960])
}

/// The unadjusted equal-power sum, before any limiting.
fn reference_mix(outgoing: &[f32], incoming: &[f32]) -> Vec<f32> {
    let frames = outgoing.len() / 2;
    let denominator = (frames - 1) as f32;
    let mut mixed = vec![0.0; outgoing.len()];

    for frame in 0..frames {
        let (out_gain, in_gain) = equal_power_gains(frame as f32 / denominator);

        for channel in 0..2 {
            let index = frame * 2 + channel;

            mixed[index] = outgoing[index] * out_gain + incoming[index] * in_gain;
        }
    }

    mixed
}

/// The per-sample gain the adaptive curve applied, where it can be measured.
fn applied_gain(output: &[f32], reference: &[f32]) -> Vec<Option<f32>> {
    output
        .iter()
        .zip(reference)
        .map(|(applied, reference)| (reference.abs() > 0.05).then(|| applied / reference))
        .collect()
}

fn assert_clean(output: &[f32]) {
    assert!(
        output
            .iter()
            .all(|sample| sample.is_finite() && sample.abs() <= 1.0),
        "output must be finite and within full scale"
    );
}

#[test]
fn validates_fade_length_and_sample_count() {
    assert!(CrossfadeWindow::from_millis(0).is_none());
    assert!(CrossfadeWindow::from_millis(999).is_none());
    assert!(CrossfadeWindow::from_millis(30_001).is_none());
    assert_eq!(
        CrossfadeWindow::from_millis(5_000).unwrap().samples(),
        240_000
    );
}

#[test]
fn equal_power_remains_the_default_curve() {
    let fade = PcmCrossfade::new(window());

    assert_eq!(fade.curve(), CrossfadeCurve::EqualPower);
    assert_eq!(fade.stats().deepest_adaptive_gain_db, 0.0);
}

#[test]
fn equal_power_fade_is_sample_accurate_and_continuous_across_arbitrary_chunks() {
    let window = CrossfadeWindow::from_millis(1_000).unwrap();
    let mut fade = PcmCrossfade::new(window);
    let mut first = vec![0.25; 2 * 18_000];
    let right = vec![0.5; 2 * 18_000];

    assert_eq!(fade.mix(&mut first, &right), Ok(18_000));
    assert!((first[0] - 0.25).abs() < 0.0001);
    assert_eq!(fade.remaining_samples(), 30_000);

    let mut second = vec![0.25; 2 * 30_000];
    let incoming = vec![0.5; 2 * 30_000];

    assert_eq!(fade.mix(&mut second, &incoming), Ok(30_000));
    assert!(fade.complete());
    assert!((second[second.len() - 1] - 0.5).abs() < 0.0001);
}

#[test]
fn equal_power_midpoint_preserves_power_for_uncorrelated_sources() {
    let mut fade = PcmCrossfade::new(CrossfadeWindow::from_millis(1_000).unwrap());
    let mut outgoing = vec![0.0; 96_000];
    let mut incoming = vec![0.0; 96_000];

    for pair in outgoing.as_chunks_mut::<2>().0 {
        pair[0] = 0.5;
    }

    for pair in incoming.as_chunks_mut::<2>().0 {
        pair[1] = 0.5;
    }

    fade.mix(&mut outgoing, &incoming).unwrap();

    for pair in outgoing.as_chunks::<2>().0 {
        assert!((pair[0] * pair[0] + pair[1] * pair[1] - 0.25).abs() < 0.00001);
    }
}

#[test]
fn equal_power_output_matches_the_reference_formula_when_in_range() {
    let (outgoing, incoming) = unrelated(-24.0, -24.0);
    let (mixed, fade) = mix(CrossfadeCurve::EqualPower, &outgoing, &incoming);
    let reference = reference_mix(&outgoing, &incoming);

    assert_eq!(fade.stats().limited_frames, 0);
    assert_eq!(mixed, reference, "in-range audio must pass through exactly");
}

#[test]
fn rejects_invalid_chunks_without_advancing() {
    for curve in CrossfadeCurve::ALL {
        let mut fade =
            PcmCrossfade::with_curve(CrossfadeWindow::from_millis(1_000).unwrap(), curve);

        assert_eq!(fade.mix(&mut [1.0], &[1.0]), Err(MixError::FrameMismatch));
        assert_eq!(
            fade.mix(&mut [1.0, 1.0], &[1.0]),
            Err(MixError::FrameMismatch)
        );
        assert_eq!(fade.consumed_samples(), 0);

        let mut too_long = vec![0.0; 2 * 48_001];

        assert_eq!(
            fade.mix(&mut too_long, &vec![0.0; 2 * 48_001]),
            Err(MixError::Complete)
        );
        assert_eq!(fade.consumed_samples(), 0);
        assert_eq!(fade.stats().limited_frames, 0);
    }
}

#[test]
fn transition_has_the_exact_requested_duration_and_endpoints() {
    let (outgoing, incoming) = unrelated(-20.0, -14.0);

    for curve in CrossfadeCurve::ALL {
        let (mixed, fade) = mix(curve, &outgoing, &incoming);

        assert_eq!(mixed.len(), outgoing.len());
        assert_eq!(fade.consumed_samples(), window().samples());
        assert_eq!(fade.remaining_samples(), 0);
        assert_eq!(window().samples(), 4 * SAMPLE_RATE);

        // The first frame is the outgoing track alone, the last the incoming.
        assert_eq!(&mixed[..2], &outgoing[..2], "{curve:?} start");
        assert_eq!(&mixed[mixed.len() - 2..], &incoming[incoming.len() - 2..]);

        let mut fade = fade;

        assert_eq!(
            fade.mix(&mut [0.0, 0.0], &[0.0, 0.0]),
            Err(MixError::Complete)
        );
    }
}

#[test]
fn adaptive_transition_is_bit_identical_to_equal_power_at_both_ends() {
    let (outgoing, incoming) = unrelated(-34.0, -8.0);
    let (equal, _) = mix(CrossfadeCurve::EqualPower, &outgoing, &incoming);
    let (adaptive, _) = mix(CrossfadeCurve::Adaptive, &outgoing, &incoming);
    let guard = 2 * 480 * 2;

    assert_eq!(adaptive[..guard], equal[..guard], "start must be seamless");
    assert_eq!(
        adaptive[adaptive.len() - guard..],
        equal[equal.len() - guard..],
        "end must be seamless"
    );
}

/// Assertions shared by both mismatched-level transitions.
fn assert_gentler_than_equal_power(outgoing_rms: f64, incoming_rms: f64) {
    let (outgoing, incoming) = unrelated(outgoing_rms, incoming_rms);
    let (equal, _) = mix(CrossfadeCurve::EqualPower, &outgoing, &incoming);
    let (adaptive, fade) = mix(CrossfadeCurve::Adaptive, &outgoing, &incoming);
    let equal_series = loudness(&equal);
    let adaptive_series = loudness(&adaptive);

    assert_clean(&adaptive);
    assert_eq!(fade.stats().limited_frames, 0);
    assert!(
        max_step(&adaptive_series) < max_step(&equal_series) - 0.15,
        "adaptive step {:.2} dB vs equal-power {:.2} dB",
        max_step(&adaptive_series),
        max_step(&equal_series)
    );

    let adaptive_error = trajectory_error(&adaptive, &outgoing, &incoming);
    let equal_error = trajectory_error(&equal, &outgoing, &incoming);

    assert!(
        adaptive_error < equal_error * 0.75,
        "adaptive tracks the level glide to {adaptive_error:.2} dB, equal-power {equal_error:.2} dB"
    );

    // The adjustment may only cut: it is never louder than equal-power.
    for (index, (adaptive, equal)) in adaptive_series.iter().zip(&equal_series).enumerate() {
        assert!(*adaptive <= *equal + 0.05, "window {index} was boosted");
    }

    // It also never exceeds the louder of the two tracks.
    let ceiling = rms_dbfs(&incoming).max(rms_dbfs(&outgoing)) + 1.5;

    assert!(adaptive_series.iter().all(|level| *level <= ceiling));
}

#[test]
fn quiet_outro_into_loud_intro_rises_more_gradually() {
    assert_gentler_than_equal_power(-32.0, -10.0);
}

#[test]
fn loud_outro_into_quiet_intro_falls_more_gradually() {
    assert_gentler_than_equal_power(-10.0, -32.0);
}

#[test]
fn two_loud_tracks_are_left_essentially_alone() {
    let (outgoing, incoming) = unrelated(-9.0, -9.0);
    let (equal, equal_fade) = mix(CrossfadeCurve::EqualPower, &outgoing, &incoming);
    let (adaptive, adaptive_fade) = mix(CrossfadeCurve::Adaptive, &outgoing, &incoming);

    assert_clean(&equal);
    assert_clean(&adaptive);
    assert!(adaptive_fade.stats().limited_frames <= equal_fade.stats().limited_frames);

    let difference = rms_dbfs(&equal) - rms_dbfs(&adaptive);

    assert!(
        (0.0..2.0).contains(&difference),
        "matched loud tracks changed by {difference:.2} dB"
    );
    assert!(peak(&adaptive) <= 1.0);
}

#[test]
fn identical_tracks_are_attenuated_by_the_correlated_excess() {
    let frames = window().samples();
    let track = music(TrackSpec::new(5, -12.0, 120.0, 130.81), frames);
    let (_, equal_fade) = mix(CrossfadeCurve::EqualPower, &track, &track);
    let (adaptive, adaptive_fade) = mix(CrossfadeCurve::Adaptive, &track, &track);
    let unlimited = reference_mix(&track, &track);

    assert_clean(&adaptive);

    // Fully correlated sources add 3 dB at the midpoint under equal-power.
    let middle = frames / 2;
    let span = SAMPLE_RATE / 4;
    let slice = |signal: &[f32]| rms_dbfs(&signal[(middle - span) * 2..(middle + span) * 2]);
    let cut = slice(&unlimited) - slice(&adaptive);

    assert!((2.0..4.0).contains(&cut), "midpoint cut was {cut:.2} dB");
    assert!(
        adaptive_fade.stats().limited_frames * 2 <= equal_fade.stats().limited_frames,
        "adaptive limited {} frames, equal-power {}",
        adaptive_fade.stats().limited_frames,
        equal_fade.stats().limited_frames
    );
}

#[test]
fn unrelated_tracks_of_equal_level_are_not_cut_at_the_midpoint() {
    let (outgoing, incoming) = unrelated(-16.0, -16.0);
    let (equal, _) = mix(CrossfadeCurve::EqualPower, &outgoing, &incoming);
    let (adaptive, _) = mix(CrossfadeCurve::Adaptive, &outgoing, &incoming);
    let middle = window().samples() / 2;
    let span = SAMPLE_RATE / 4;
    let slice = |signal: &[f32]| rms_dbfs(&signal[(middle - span) * 2..(middle + span) * 2]);

    assert!(
        (slice(&equal) - slice(&adaptive)).abs() < 1.5,
        "equal-level unrelated tracks moved by {:.2} dB",
        slice(&equal) - slice(&adaptive)
    );
}

#[test]
fn silence_stays_silent_and_unadjusted() {
    let frames = window().samples();
    let quiet = silence(frames);

    for curve in CrossfadeCurve::ALL {
        let (mixed, fade) = mix(curve, &quiet, &quiet);

        assert!(mixed.iter().all(|sample| *sample == 0.0), "{curve:?}");
        assert_eq!(fade.stats().deepest_adaptive_gain_db, 0.0);
        assert_eq!(fade.stats().limited_frames, 0);
    }
}

#[test]
fn near_silence_is_never_chased() {
    let frames = window().samples();
    let hiss_a = noise_floor(3, -92.0, frames);
    let hiss_b = noise_floor(4, -88.0, frames);
    let (equal, _) = mix(CrossfadeCurve::EqualPower, &hiss_a, &hiss_b);
    let (adaptive, fade) = mix(CrossfadeCurve::Adaptive, &hiss_a, &hiss_b);

    assert_eq!(adaptive, equal, "noise floors must not drive any gain");
    assert_eq!(fade.stats().deepest_adaptive_gain_db, 0.0);
}

#[test]
fn a_silent_side_disables_the_adjustment_entirely() {
    let frames = window().samples();
    let track = music(TrackSpec::new(41, -12.0, 110.0, 98.0), frames);
    let quiet = silence(frames);

    for (outgoing, incoming) in [(&track, &quiet), (&quiet, &track)] {
        let (equal, _) = mix(CrossfadeCurve::EqualPower, outgoing, incoming);
        let (adaptive, fade) = mix(CrossfadeCurve::Adaptive, outgoing, incoming);

        assert_eq!(adaptive, equal);
        assert_eq!(fade.stats().deepest_adaptive_gain_db, 0.0);
    }
}

#[test]
fn a_track_ending_early_is_handled_like_silence() {
    // The worker substitutes zeros when the outgoing decoder underruns.
    let frames = window().samples();
    let mut outgoing = music(TrackSpec::new(61, -14.0, 100.0, 110.0), frames);
    let incoming = music(TrackSpec::new(62, -14.0, 100.0, 123.47), frames);

    outgoing[frames..].fill(0.0);

    let (mixed, fade) = mix(CrossfadeCurve::Adaptive, &outgoing, &incoming);

    assert_clean(&mixed);
    assert!(fade.stats().deepest_adaptive_gain_db.abs() <= MAX_CUT_DB as f32);
}

#[test]
fn a_sudden_peak_does_not_move_the_adaptive_gain() {
    let frames = window().samples();
    let outgoing = music(TrackSpec::new(71, -22.0, 100.0, 110.0), frames);
    let incoming = music(TrackSpec::new(72, -22.0, 120.0, 123.47), frames);
    let spiked = with_clicks(incoming.clone(), &[frames / 2, frames / 2 + 9_000], 1.0);
    let (plain, plain_fade) = mix(CrossfadeCurve::Adaptive, &outgoing, &incoming);
    let (clicked, clicked_fade) = mix(CrossfadeCurve::Adaptive, &outgoing, &spiked);

    assert_clean(&clicked);
    assert!(
        (plain_fade.stats().deepest_adaptive_gain_db
            - clicked_fade.stats().deepest_adaptive_gain_db)
            .abs()
            < 0.2
    );

    // Away from the clicks the two renders agree closely.
    let start = frames / 2 + 2_000;
    let end = frames / 2 + 8_000;

    for index in start * 2..end * 2 {
        assert!(
            (plain[index] - clicked[index]).abs() < 0.01,
            "sample {index} differs"
        );
    }
}

/// Distortion of a 220 Hz sine as the ratio of everything else to the
/// fundamental, over `cycles` whole cycles (4,800 frames per 22 cycles).
fn sine_distortion(signal: &[f32], start: usize) -> f64 {
    let frames = 4_800;
    let (mut sine, mut cosine, mut power) = (0.0, 0.0, 0.0);

    for index in 0..frames {
        let value = f64::from(signal[(start + index) * 2]);
        let angle = std::f64::consts::TAU * 220.0 * (start + index) as f64 / SAMPLE_RATE as f64;

        sine += value * angle.sin();
        cosine += value * angle.cos();
        power += value * value;
    }

    let fundamental = 2.0 * (sine * sine + cosine * cosine) / (frames as f64 * frames as f64);
    let total = power / frames as f64;

    ((total - fundamental).max(0.0) / fundamental).sqrt()
}

#[test]
fn peaks_are_limited_smoothly_instead_of_clipped() {
    // Two identical near-full-scale sine waves reach 1.39x full scale mid-fade.
    let frames = window().samples();
    let mut tone = vec![0.0; frames * 2];

    for frame in 0..frames {
        let angle = std::f64::consts::TAU * 220.0 * frame as f64 / SAMPLE_RATE as f64;

        tone[frame * 2] = angle.sin() as f32 * 0.98;
        tone[frame * 2 + 1] = angle.sin() as f32 * 0.98;
    }

    let reference = reference_mix(&tone, &tone);
    let clipped: Vec<f32> = reference
        .iter()
        .map(|sample| sample.clamp(-1.0, 1.0))
        .collect();
    let mut reductions = Vec::new();

    assert!(peak(&reference) > 1.3, "the fixture must overshoot");

    for curve in CrossfadeCurve::ALL {
        let (mixed, fade) = mix(curve, &tone, &tone);

        assert_clean(&mixed);
        assert!(fade.stats().limited_frames > 0, "{curve:?} saw overs");

        // Compare harmonic distortion where the mix is well over full scale.
        for start in [60_000, 96_000, 120_000] {
            let limited = sine_distortion(&mixed, start);
            let hard = sine_distortion(&clipped, start);

            assert!(
                limited * 10.0 < hard,
                "{curve:?} distortion {limited:.4} vs hard clipping {hard:.4} at {start}"
            );
        }

        reductions.push(fade.stats().peak_reduction_db);
    }

    assert!(
        reductions[1] < reductions[0],
        "adaptive should leave the limiter less to do: {reductions:?}"
    );
}

#[test]
fn output_is_bit_identical_for_any_chunking() {
    let (outgoing, incoming) = unrelated(-30.0, -9.0);
    let patterns: [&[usize]; 6] = [
        &[960],
        &[1],
        &[7, 13, 480, 481, 1_023, 2],
        &[479, 1, 480, 481],
        &[window().samples()],
        &[960, 17, 4_800, 3, 96_000],
    ];

    for curve in CrossfadeCurve::ALL {
        let (expected, expected_fade) = mix_chunked(curve, &outgoing, &incoming, patterns[0]);

        for pattern in &patterns[1..] {
            let (mixed, fade) = mix_chunked(curve, &outgoing, &incoming, pattern);

            assert_eq!(mixed, expected, "{curve:?} differs for {pattern:?}");
            assert_eq!(fade.stats(), expected_fade.stats());
        }
    }
}

#[test]
fn applied_gain_is_continuous_bounded_and_never_a_boost() {
    for (out_rms, in_rms) in [
        (-38.0, -14.0),
        (-14.0, -38.0),
        (-18.0, -18.0),
        (-26.0, -16.0),
    ] {
        let (outgoing, incoming) = unrelated(out_rms, in_rms);
        let (mixed, fade) = mix(CrossfadeCurve::Adaptive, &outgoing, &incoming);
        let reference = reference_mix(&outgoing, &incoming);
        let gains = applied_gain(&mixed, &reference);
        let floor = 10f32.powf(-(MAX_CUT_DB as f32) / 20.0);

        assert_eq!(fade.stats().limited_frames, 0, "limiter must stay out");

        let mut previous: Option<f32> = None;
        let mut deepest = 1.0_f32;

        for gain in gains.iter().flatten() {
            assert!(*gain <= 1.0 + 1.0e-4, "boost of {gain}");
            assert!(*gain >= floor - 1.0e-3, "cut deeper than the bound: {gain}");

            deepest = deepest.min(*gain);
        }

        for channel in 0..2 {
            for frame in gains.as_chunks::<2>().0.iter().map(|frame| frame[channel]) {
                if let (Some(before), Some(now)) = (previous, frame) {
                    assert!(
                        (now - before).abs() < 2.0e-4,
                        "gain jumped from {before} to {now} ({out_rms} -> {in_rms})"
                    );
                }

                previous = frame;
            }

            previous = None;
        }

        // Real adjustment happened for mismatched levels.
        if (out_rms - in_rms).abs() > 15.0 {
            assert!(deepest < 0.9, "no adjustment for {out_rms} -> {in_rms}");
        }
    }
}

#[test]
fn stereo_image_is_preserved() {
    let (outgoing, incoming) = unrelated(-30.0, -9.0);
    let reference = reference_mix(&outgoing, &incoming);
    let (mixed, fade) = mix(CrossfadeCurve::Adaptive, &outgoing, &incoming);

    assert_eq!(fade.stats().limited_frames, 0);

    // One common gain per frame means left/right proportions are untouched.
    for (frame, reference_frame) in mixed
        .as_chunks::<2>()
        .0
        .iter()
        .zip(reference.as_chunks::<2>().0)
    {
        let cross_left = frame[0] * reference_frame[1];
        let cross_right = frame[1] * reference_frame[0];

        assert!((cross_left - cross_right).abs() < 1.0e-6);
    }

    assert!(
        (stereo_correlation(&mixed) - stereo_correlation(&reference)).abs() < 0.01,
        "stereo correlation moved"
    );
}

#[test]
fn a_single_channel_signal_stays_in_its_channel() {
    let frames = window().samples();
    let mut outgoing = music(TrackSpec::new(81, -30.0, 100.0, 110.0), frames);
    let mut incoming = music(TrackSpec::new(82, -9.0, 130.0, 146.83), frames);

    for frame in 0..frames {
        outgoing[frame * 2 + 1] = 0.0;
        incoming[frame * 2 + 1] = 0.0;
    }

    let (mixed, _) = mix(CrossfadeCurve::Adaptive, &outgoing, &incoming);

    assert!(mixed.as_chunks::<2>().0.iter().all(|frame| frame[1] == 0.0));
    assert!(mixed.as_chunks::<2>().0.iter().any(|frame| frame[0] != 0.0));
}

#[test]
fn invalid_samples_are_silenced_and_never_poison_later_audio() {
    let (mut outgoing, incoming) = unrelated(-20.0, -14.0);

    for position in [0, 1, 5_000, 5_001, 90_000, outgoing.len() - 2] {
        outgoing[position] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY][position % 3];
    }

    for curve in CrossfadeCurve::ALL {
        let (mixed, fade) = mix(curve, &outgoing, &incoming);

        assert_clean(&mixed);
        assert!(fade.stats().deepest_adaptive_gain_db.is_finite());
        assert!(mixed[5_000] == 0.0 || mixed[5_000].is_finite());
    }
}

#[test]
fn extreme_finite_input_stays_bounded() {
    let frames = window().samples();
    let loud = vec![3.0e38_f32; frames * 2];
    let louder = vec![-3.0e38_f32; frames * 2];

    for curve in CrossfadeCurve::ALL {
        let (mixed, fade) = mix(curve, &loud, &louder);

        assert_clean(&mixed);
        assert!(fade.stats().deepest_adaptive_gain_db.is_finite());
    }
}

#[test]
fn the_limiter_releases_before_the_end_of_the_window() {
    // A click just before the end must not leave a gain step behind.
    let frames = window().samples();
    let outgoing = vec![0.9; frames * 2];
    let incoming = vec![0.9; frames * 2];
    let clicked = with_clicks(incoming.clone(), &[frames - 100], 1.0);
    let (mixed, _) = mix(CrossfadeCurve::EqualPower, &outgoing, &clicked);

    assert_clean(&mixed);
    assert!((mixed[mixed.len() - 2] - 0.9).abs() < 1.0e-6);
    assert!((mixed[mixed.len() - 1] - 0.9).abs() < 1.0e-6);
}

#[test]
fn the_shortest_window_is_supported() {
    let window = CrossfadeWindow::from_millis(1_000).unwrap();
    let outgoing = music(TrackSpec::new(91, -28.0, 100.0, 110.0), window.samples());
    let incoming = music(TrackSpec::new(92, -10.0, 100.0, 123.47), window.samples());
    let mut fade = PcmCrossfade::with_curve(window, CrossfadeCurve::Adaptive);
    let mut mixed = outgoing.clone();

    fade.mix(&mut mixed, &incoming).unwrap();

    assert!(fade.complete());
    assert_clean(&mixed);
    assert_eq!(&mixed[mixed.len() - 2..], &incoming[incoming.len() - 2..]);
}

#[test]
fn a_window_that_is_not_a_whole_number_of_hops_still_ends_seamlessly() {
    // 1_337 ms is 64_176 frames: not a multiple of the 480-frame hop.
    let window = CrossfadeWindow::from_millis(1_337).unwrap();
    let outgoing = music(TrackSpec::new(93, -30.0, 100.0, 110.0), window.samples());
    let incoming = music(TrackSpec::new(94, -9.0, 100.0, 123.47), window.samples());
    let reference = reference_mix(&outgoing, &incoming);
    let mut fade = PcmCrossfade::with_curve(window, CrossfadeCurve::Adaptive);
    let mut mixed = outgoing.clone();

    for (out, inc) in mixed.chunks_mut(2 * 777).zip(incoming.chunks(2 * 777)) {
        fade.mix(out, inc).unwrap();
    }

    let tail = mixed.len() - 2 * 480;

    assert!(fade.complete());
    assert_eq!(mixed[tail..], reference[tail..]);
}
