# Adaptive crossfade measurements

Compares the `adaptive` and `equalPower` crossfade curves in `remedia-core`. Everything here is synthetic and
deterministic: no decoder, network, or Discord connection is involved, and the audio can be regenerated
bit-for-bit from the fixture parameters in `crates/remedia-core/src/crossfade/fixtures.rs`.

## Reproduction

```sh
export PKG_CONFIG_PATH=/opt/homebrew/opt/ffmpeg/lib/pkgconfig   # this machine; adjust for yours
cargo build --release -p remedia-core --example crossfade_adaptive
./target/release/examples/crossfade_adaptive bench
./target/release/examples/crossfade_adaptive render <output-dir>
```

`render` writes `<scenario>-<curve>.wav` (2 s of the outgoing track, the 5 s overlap, 2 s of the incoming track,
16-bit stereo 48 kHz) and `summary.md` into the directory. The WAV files are not checked in (about 1.7 MB each);
regenerate them to listen. Unit tests over the same fixtures run with `cargo test -p remedia-core --lib crossfade`.

## Environment

Apple M5, macOS Darwin 27 arm64, Rust 1.99.1, release profile. The host was not isolated. Numbers below come from one
run; three consecutive `bench` runs agreed to within 0.1 ns per frame.

## Mixing cost

Median of 9 repeats, each mixing 6 x 5000 ms of 48 kHz stereo in 960-frame blocks.

| Case                                          | ns / frame | Real-time CPU (1 stream) | Allocations / run | Bytes / run |
| --------------------------------------------- | ---------: | -----------------------: | ----------------: | ----------: |
| quiet-into-loud (limiter idle): copy baseline |        0.1 |                   0.000% |                 0 |           0 |
| quiet-into-loud (limiter idle): equalPower    |        2.7 |                   0.013% |                 0 |           0 |
| quiet-into-loud (limiter idle): adaptive      |        3.5 |                   0.017% |                 0 |           0 |
| loud-into-loud (limiter busy): copy baseline  |        0.1 |                   0.000% |                 0 |           0 |
| loud-into-loud (limiter busy): equalPower     |        3.0 |                   0.014% |                 0 |           0 |
| loud-into-loud (limiter busy): adaptive       |        3.7 |                   0.018% |                 0 |           0 |

`ns / frame` is the median of nine repeats of six 5 s overlaps in 960-frame blocks, including the copy that refreshes
the in-place buffer (the baseline row shows that copy costs about 0.1 ns). "Real-time CPU" is the share of one core
needed to produce one live 48 kHz stream. "Allocations" counts every heap allocation made during one 5 s overlap,
including constructing the crossfade, using a counting global allocator. Adaptive adds about 0.7 ns per frame. These
numbers cover the mixer only, not decoding, Opus encoding, or the playback worker.

## Transitions

| Scenario           | Curve      | Overlap RMS |  Peak | Largest 100 ms loudness step | Glide error | Deepest adaptive cut | Frames limited | Deepest limiting |
| ------------------ | ---------- | ----------: | ----: | ---------------------------: | ----------: | -------------------: | -------------: | ---------------: |
| quiet-into-loud    | equalPower |  -12.9 dBFS | 0.861 |                      1.79 dB |     6.08 dB |              0.00 dB |              0 |          0.00 dB |
| quiet-into-loud    | adaptive   |  -15.7 dBFS | 0.853 |                      1.55 dB |     2.29 dB |             -6.00 dB |              0 |          0.00 dB |
| loud-into-quiet    | equalPower |  -13.1 dBFS | 0.873 |                      1.62 dB |     5.98 dB |              0.00 dB |              0 |          0.00 dB |
| loud-into-quiet    | adaptive   |  -15.3 dBFS | 0.859 |                      1.59 dB |     2.05 dB |             -6.00 dB |              0 |          0.00 dB |
| loud-into-loud     | equalPower |   -9.1 dBFS | 1.000 |                      0.62 dB |     0.13 dB |              0.00 dB |          12700 |          2.05 dB |
| loud-into-loud     | adaptive   |   -9.2 dBFS | 1.000 |                      0.64 dB |     0.16 dB |             -0.51 dB |           9495 |          1.79 dB |
| identical-tracks   | equalPower |   -9.9 dBFS | 0.989 |                      1.08 dB |     2.17 dB |              0.00 dB |              0 |          0.00 dB |
| identical-tracks   | adaptive   |  -11.7 dBFS | 0.783 |                      1.02 dB |     0.41 dB |             -2.98 dB |              0 |          0.00 dB |
| silence-into-music | equalPower |  -14.9 dBFS | 0.685 |                      3.65 dB |         n/a |              0.00 dB |              0 |          0.00 dB |
| silence-into-music | adaptive   |  -14.9 dBFS | 0.685 |                      3.65 dB |         n/a |              0.00 dB |              0 |          0.00 dB |
| noise-floor        | equalPower |  -90.0 dBFS | 0.000 |                      0.05 dB |         n/a |              0.00 dB |              0 |          0.00 dB |
| noise-floor        | adaptive   |  -90.0 dBFS | 0.000 |                      0.05 dB |         n/a |              0.00 dB |              0 |          0.00 dB |
| sudden-peaks       | equalPower |  -22.0 dBFS | 1.000 |                      1.74 dB |     0.19 dB |              0.00 dB |           8944 |          3.01 dB |
| sudden-peaks       | adaptive   |  -22.1 dBFS | 1.000 |                      1.70 dB |     0.27 dB |             -0.49 dB |           8749 |          2.65 dB |

- **Overlap RMS** and **Peak** are for the mixed 5 s overlap only.
- **Largest 100 ms loudness step** is the largest change between consecutive 400 ms RMS windows taken every 100 ms.
- **Glide error** is the mean distance between the output's 400 ms loudness and a smoothstep glide, in decibels, from
  the outgoing track's own level to the incoming track's own level. It measures how closely the controller tracks
  the target it is designed around. It is not a perceptual score, and was not validated by a listening test.
- **Deepest adaptive cut** is the largest attenuation the adaptive curve scheduled. The bound is -6 dB.
- **Frames limited** counts frames the peak limiter attenuated by more than 0.1 dB, out of 240,000.

Scenarios:

- `quiet-into-loud`: quiet outro (-32 dBFS RMS) into a loud intro (-10 dBFS RMS)
- `loud-into-quiet`: loud outro (-10 dBFS RMS) into a quiet intro (-32 dBFS RMS)
- `loud-into-loud`: two loud masters (-9 dBFS RMS, peaks at 0.98)
- `identical-tracks`: the same loud track overlapped with itself, sample-aligned (fully correlated)
- `silence-into-music`: digital silence into a loud intro
- `noise-floor`: two -90 dBFS dither tails
- `sudden-peaks`: matched tracks with coincident full-scale clicks mid-fade (1.41x full scale)

## Reading the results

- For mismatched levels (22 dB apart) the adaptive glide error is roughly a third of equal-power's. The largest
  loudness step shrinks by 0.03 dB (loud into quiet) to 0.24 dB (quiet into loud); most of the gain is in where the
  level change happens, not its peak rate.
- Matched, uncorrelated tracks are left nearly alone (at most 0.5 dB), and the glide error is marginally worse than
  equal-power there (0.13 to 0.16 dB, 0.19 to 0.27 dB with clicks) because the cut tracks correlation rather than
  the glide.
- Fully correlated tracks (the same track twice, sample-aligned) are cut by 2.98 dB, which is the 3.01 dB that two
  coherent equal-level sources add at the midpoint. The peak falls from 0.989 to 0.783 and the glide error from 2.17 to
  0.41 dB.
- Digital silence and -90 dBFS noise floors produce no adjustment, and output is identical to `equalPower`.
- With two loud masters or coincident full-scale clicks the limiter engages for a few percent of frames, by up to
  about 2 to 3 dB; adaptive reduces how often and how deeply, but does not remove it.

## Limitations

- The signals are synthetic (kick, bass, pad and hi-hat, soft-saturated and limited to a target RMS). They are
  music-like, not music, and results on real masters will differ.
- Levels are RMS, not LUFS, and there is no frequency weighting.
- The limiter has no look-ahead; a single-sample click that sums beyond full scale is attenuated from that sample.
- One machine, one run per configuration.
