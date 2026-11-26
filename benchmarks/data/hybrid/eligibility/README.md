# Hybrid eligibility raw results

See [the technical report](../../../hybrid-eligibility-results.md) for interpretation,
limitations and reproducible commands. Original baseline artifacts and earlier result
folders remain intact. JSONL rows are unmodified child results plus parent OS sampling;
manifest files record arguments, binary hashes and fixture hashes.

| Final suite             | Rows | Product source | Scope                                                                         |
| ----------------------- | ---: | -------------- | ----------------------------------------------------------------------------- |
| final-mac-tuning        |  462 | 7e489aa        | File MP3/Opus, 1–512 sessions, workers 2/4/10, budgets 1/8/32                 |
| final-mac-controls      |   27 | 7e489aa        | Corrected dedicated versus scheduled; four paused seeks/graph updates         |
| final-mac-mixed         |   48 | 7e489aa        | Staggered independent MP3/Opus/AAC file contexts, PCM/Opus                    |
| final-mac-sustained     |    9 | 7e489aa        | 128 sessions, 15-second realtime Opus files                                   |
| final-mac-transport     |  162 | 7e489aa        | Actual sender/router, bounded controlled async consumer; no network/DAVE cost |
| final-mac-post-boundary |   48 | b763e7e        | Fresh current file comparison after oversized-packet bound fix                |
| final-linux-2cpu        |   96 | b763e7e        | Quota 2, affinity 0/1; paced file workloads                                   |
| final-linux-4cpu        |   96 | b763e7e        | Quota 4, affinity 0/1/2/3; paced file workloads                               |

These 948 rows pass complete sample counts, cross-repetition and cross-backend payload
checksums, natural completion and explicit scheduler admission/fallback checks. Every
scheduled row has zero fallbacks and probe retries. The final filter-shape admission
guard at 158c7f3 does not disqualify any of these workloads. Variable-output filters
remain dedicated and are tested separately; their fallbacks are not performance evidence.

`summary-final.json` contains three-run aggregates. `qualification-final.json` records
whole-dataset delivery checks. Recreate both using:

```sh
python3 benchmarks/hybrid-eligibility-summary.py benchmarks/data/hybrid/eligibility --output benchmarks/data/hybrid/eligibility/summary-final.json --qualification-output benchmarks/data/hybrid/eligibility/qualification-final.json
```

Environment and validation JSON files record hardware/native dependencies and exact
check scope. `stalled-plugin-fallback.json` is a separate diagnostic: 16 producer-stream
fallbacks, zero admissions, not scheduler performance. Caller-driver threads are identified.

Preserved earlier/excluded material:

- `mac-file-budgets`: 462 earlier e556ef6 ordinary file runs, before seek correction.
- `mac-controls`: partial pilot; original main timed out behind the benchmark start gate.
- `mac-controls-v2`: partial pilot; repeated MP3 seeks produced inconsistent PCM checksums.
- `control-diagnostics`: reproduces seek-history inconsistency with file and owned bytes.
- Failure notes/stderr preserve those rejected runs. They do not enter final aggregates.
- `linux-live-mount-build-failure`: rejected build with a truncated Docker mount view;
  immutable source snapshots were used for all accepted Linux measurements.

The 2 ms parent sampler can miss transient thread/RSS peaks. Worker busy nanoseconds
are elapsed action time, including preemption. Delivery gaps/lateness are timing proxies;
no physical-device underrun, live Discord, hours-long stability or native allocation
profile is claimed. Performance results deliberately retain overload and regressions.
