# Phase 2 recorded measurements

Three-run medians, paced eligible Opus with crossfade policy enabled and no staged track. CPU is process user + system seconds; these are short 2.02-second fixtures. Each cell is phase 1 → phase 2.

| Sessions | CPU seconds    | RSS KiB                 | OS threads        | Startup p99 µs    | Sink delay p95 µs | Sink delay p99 µs  |
| -------- | -------------- | ----------------------- | ----------------- | ----------------- | ----------------- | ------------------ |
| 1        | 0.092 → 0.036  | 13680.000 → 13120.000   | 2.000 → 2.000     | 845.000 → 624.000 | 216.000 → 257.000 | 252.000 → 266.000  |
| 8        | 0.376 → 0.108  | 17760.000 → 14592.000   | 9.000 → 9.000     | 656.000 → 366.000 | 248.000 → 254.000 | 256.000 → 262.000  |
| 32       | 0.637 → 0.263  | 31264.000 → 18928.000   | 33.000 → 33.000   | 657.000 → 355.000 | 212.000 → 235.000 | 251.000 → 256.000  |
| 64       | 1.131 → 0.575  | 49264.000 → 24608.000   | 65.000 → 65.000   | 444.000 → 712.000 | 226.000 → 235.000 | 253.000 → 330.000  |
| 128      | 2.283 → 1.031  | 85152.000 → 35872.000   | 129.000 → 129.000 | 403.000 → 261.000 | 235.000 → 243.000 | 253.000 → 1141.000 |
| 256      | 4.726 → 2.369  | 156960.000 → 58672.000  | 257.000 → 257.000 | 430.000 → 244.000 | 235.000 → 238.000 | 255.000 → 269.000  |
| 512      | 11.159 → 6.329 | 298912.000 → 102608.000 | 513.000 → 513.000 | 669.000 → 331.000 | 242.000 → 247.000 | 281.000 → 349.000  |

## Longer filtered playback with command churn

One run per count, generated 15-second mono WAV, native tremolo filtering, complexity 9 / 128 kbps / VBR. Each session pauses immediately after play, then seeks to 100 ms while paused and resumes. 14.9 seconds per session are delivered. Startup includes the intentionally paused setup period, so it does not isolate decoder startup. Command percentiles combine pause, seek, and resume calls. The benchmark adds one resource sampler thread. Footprint is the maximum of one-second native samples, not USS or peak RSS.

| Sessions | Wall s | CPU s  | Mean process CPU % | Sampled RSS KiB | Sampled footprint B | Threads | Startup p99 µs | Sink p95 / p99 µs | Command p95 / p99 µs | Max collection ms |
| -------- | ------ | ------ | ------------------ | --------------- | ------------------- | ------- | -------------- | ----------------- | -------------------- | ----------------- |
| 1        | 14.898 | 0.531  | 3.30               | 15504           | 4391464             | 3       | 15018          | 229 / 252         | 10 / 10              | 0.067333          |
| 8        | 14.962 | 2.884  | 18.86              | 27344           | 15761984            | 10      | 66398          | 265 / 457         | 76 / 86              | 0.048625          |
| 32       | 15.163 | 6.066  | 39.74              | 65264           | 53150320            | 34      | 266135         | 245 / 257         | 101 / 125            | 0.029500          |
| 64       | 15.440 | 8.425  | 54.39              | 116016          | 102957752           | 66      | 542991         | 203 / 249         | 77 / 89              | 0.039000          |
| 128      | 15.977 | 16.504 | 103.23             | 217728          | 202670968           | 130     | 1065902        | 221 / 250         | 80 / 89              | 0.037542          |
| 256      | 17.174 | 33.981 | 197.86             | 422560          | 270648544           | 258     | 2241473        | 666 / 6603        | 92 / 125             | 0.088833          |
| 512      | 19.982 | 79.261 | 392.70             | 828624          | 667895704           | 514     | 4985279        | 251 / 411         | 128 / 203            | 0.122541          |
