# Audio fixtures

Small generated fixtures. None contains third-party audio.

- `generated-*.ogg`, `generated-tone.mp3`: synthetic streams used by the playback tests.
  `generated-opus-passthrough-long.ogg` repeats the packets of `generated-opus-passthrough.ogg`
  four times (`python3 benchmarks/make_opus_fixture.py IN OUT --loops 4`); like the original it has
  pre-skip 0.
- `ffmpeg-opus-*`: real Opus streams written by FFmpeg and libopus, so they carry a genuine
  312-sample pre-skip, `skip_samples` side data on the first packet (Ogg), millisecond timestamps
  (WebM), and end trimming on the last packet. They cover a stream that starts abruptly (audible
  pre-roll) and one with 300 ms of silent lead-in (inaudible pre-roll). Regenerate them with
  FFmpeg 7 or newer built with libopus (these were written by 7.1):

  ```sh
  SRC='aevalsrc=0.2*sin(2*PI*220*t)*sin(2*PI*0.5*t)+0.15*sin(2*PI*330*t+sin(2*PI*3*t))+0.1*sin(2*PI*(440+30*sin(0.3*t))*t)|0.2*sin(2*PI*277*t)*sin(2*PI*0.7*t)+0.15*sin(2*PI*392*t)+0.1*random(0)-0.05:s=48000:d=1.5'
  ffmpeg -f lavfi -i "$SRC" -c:a libopus -b:a 96k ffmpeg-opus-abrupt-start.ogg
  ffmpeg -f lavfi -i "$SRC" -af adelay=300\|300 -c:a libopus -b:a 96k ffmpeg-opus-silent-lead.ogg
  ffmpeg -f lavfi -i "$SRC" -af adelay=300\|300 -c:a libopus -b:a 96k ffmpeg-opus-silent-lead.webm
  # ffmpeg-opus-silent-lead-long.ogg: the same source with d=6 and -b:a 64k, for promotion tests
  ```

  The source contains a pseudo-random term, so a different FFmpeg build can produce different
  bytes; the tests assert properties, not hashes.
