# Native CI and release audit

Measured November 16, 2026. Applies after v0.3.0; its public tag and all 35 assets
remain unchanged. Raw local profile/cache measurements: [ci-release-profiles.json](../benchmarks/ci-release-profiles.json).

## Hosted baseline

Actions run [43813499144](https://github.com/twlite/remedia/actions/runs/43813499144)
(v0.3.0) included a failed upload and a rerun: job timestamps span 81m13s. This is
not a clean single-attempt wall-clock benchmark. Its slowest native build was
macOS x64 at 62m37s. Earlier successful release
[v0.2.2 / 43584604353](https://github.com/twlite/remedia/actions/runs/43584604353)
spanned 34m30s. It used dynamic dependencies and fewer targets, so the timings
are historical context rather than a controlled static-build comparison.

| Step                                               |         v0.3.0 |         v0.2.2 |
| -------------------------------------------------- | -------------: | -------------: |
| macOS x64 Rust release tests                       |         27m02s |          5m52s |
| macOS x64 server / addon builds                    | 6m46s / 11m12s |  2m17s / 1m02s |
| Windows x64 native dependency setup                |         14m14s |            37s |
| Windows ARM64 native dependency setup              |         21m16s | not comparable |
| macOS x64 native dependency setup                  |          8m24s |          2m31s |
| Package job                                        |          5m17s |          1m09s |
| Source assembly inside package job                 |          1m39s |         absent |
| Upload job                                         |          3m11s |            22s |
| Download release-server artifact inside upload job |          1m46s | not comparable |

Fast lint/type/unit checks run independently of the Rust/source/docs job, so they
can report failures before native compilation. Expensive jobs wait for these fast
checks instead of spending native build time on a failing lint/type/script run.

Essential: native codec/filter/seek/crossfade tests, binary inspection, platform
runtime checks, license/source validation, immutable version/tag checks, package
inspection and serialized publication. Removed: obsolete bootstrap crate/discovery,
install-name rewriting, full source/build/download caches per platform, routine
unpublished server probes, release-profile test linking, and duplicate Node-version
jobs on PRs. Feature branches use the PR trigger instead of duplicating it with
`fix/**` push runs. Release coverage remains all 13 addons, eight servers and Node 22.18/24/26.

Server builds previously used Cargo's implicit host target while NAPI used an
explicit target, duplicating target artifacts. Both now use the same target/profile
path. Rust tests use the test profile; optimized PR artifacts use `ci` (no LTO).
TypeScript-only integration changes skip additional cross builds and all-target
packing; low-level inputs and releases still require them.

## Local profile measurements

Apple ARM64, Rust 1.99.1, static FFmpeg revision 946fcce, existing native prefix,
separate initially empty Cargo target directories. Commands build remedia-server
and remedia-node together; no-change rebuild is the warm column. Fat was repeated
because an earlier sample overlapped native compilation; the repeated sample is
reported here. This host was not isolated; treat small differences as noise.

| Release LTO | Cold Rust build | Warm no-change build | CLI MiB | Runtime MiB | Addon MiB |
| ----------- | --------------: | -------------------: | ------: | ----------: | --------: |
| fat         |          78.96s |                0.30s |    4.24 |       22.54 |     21.27 |
| thin        |          55.18s |                0.14s |    5.76 |       27.84 |     26.05 |
| off         |          34.61s |                0.14s |    6.55 |       30.39 |     28.15 |

Retain fat LTO for distribution: thin increases the addon by 22.5%, and off by
32.4%. PRs use off to reduce compilation cost; no codec, filter, encoder quality,
threading, Cargo optimization level, or playback implementation was reduced.

Five fresh-process repetitions per case, 32 sessions, existing generated MP3 and
Opus fixtures, passthrough/filter, unpaced controlled sink. All variants delivered
identical packet and media sample totals. For filtered Opus, median child CPU was
0.455/0.453/0.479s (fat/thin/off); peak RSS 26.36/26.20/27.77 MiB; throughput
1098/1034/1023 aggregate audio seconds per wall second. These short synthetic
workloads do not establish production throughput, sustained memory, or live Discord
performance. The decision prioritizes release size and removes LTO from tests/PRs.

Reproduce using the pinned native prefix and toolchain:

```sh
for mode in fat thin off; do
  CARGO_TARGET_DIR="/tmp/remedia-profile-$mode" CARGO_PROFILE_RELEASE_LTO="$mode" \
    cargo build --release --locked -p remedia-server -p remedia-node
  # Repeat unchanged for warm time; build the existing core benchmark in that directory.
  CARGO_TARGET_DIR="/tmp/remedia-profile-$mode" CARGO_PROFILE_RELEASE_LTO="$mode" \
    cargo build --release --locked -p remedia-core --example concurrent_playback_benchmark
  python3 benchmarks/playback.py --binary "/tmp/remedia-profile-$mode/release/examples/concurrent_playback_benchmark" \
    --counts 32 --repeats 5 --workloads passthrough,filter --pacing unpaced --output "/tmp/$mode.jsonl"
done
```

## Cache and source results

The POSIX dependency recipe measured 73.23s cold and 0.153s on a warm prefix,
including the configuration audit. This is a local installed-prefix hit, not a
measurement of Actions cache transfer time. Windows vcpkg compilation remains
unmeasured after the change. It uses the exact pinned overlay in classic mode,
avoiding historical Git object resolution/full-history downloads. Only installed
libraries/metadata/configuration are cached, never buildtrees/download caches.
Audited prefixes are saved before Rust/tests, so later job failures preserve them.
FreeBSD caches its prefix across VM runs and independently fingerprints its actual
compiler before reuse. All cache hits retain dependency/configuration audits.
The first hosted FreeBSD run exhausted its VM disk while linking full-DWARF test
binaries. That job now disables dev/test debug symbols (retaining debug assertions
and the full test suite), clears downloaded pkg archives, and removes completed
test-profile outputs before optimized artifact builds. Disk usage is logged around
the test stage. Release optimization and native dependency settings are unchanged.

The retained dependency snapshot is 119,578,611 bytes (114.04 MiB), versus
1,965,145,303 bytes (1.83 GiB) for v0.3.0's combined archive: 93.9% smaller even
before reuse. Future unchanged dependency inputs reuse that public asset with no
source archive download, vendoring, generation, upload, or transfer between jobs.
Target jobs transfer small generated configuration/compiler records only. Each
release adds its small Remedia source/configuration archive and manifest. A local
all-target **packaging fixture** produced a 1,599,132-byte (1.53 MiB) release-source archive;
its configurations are fixtures, so an actual future release size remains unmeasured.

The source mechanism is implemented in corresponding-source.mjs and documented in
[SOURCE_DISTRIBUTION.md](../SOURCE_DISTRIBUTION.md). It retains complete locked
Cargo sources, native sources, port patches and exact controlling scripts. Unused
vendored test binaries and unsupported-target objects are omitted; NAPI's embedded
helper is regenerated from its retained C source before offline checks. Necessary
MSVC System Library import stubs remain. Dependency snapshots and immutable
release-specific source assets become public before npm; anonymous API metadata,
SHA-256 identities and all target records are checked before publishing binaries.
No source snapshot or new binary release was published to test this change.

The historical no-op `--no-ffmpeg` / `-NoFFmpeg` installer switches remain because
released v0.3.0 server updaters can pass them. Removing them would break an
existing supported updater. They perform no dependency installation. The unused
build verifier's external-FFmpeg runtime mode and macOS dylib rewriting are removed.

## Installation layout

Both installers stage complete immutable pairs and activate canonical `libexec/vVERSION`
directories atomically, with cached hash verification and a retained previous version.
Legacy tmp/GUID directories migrate without stopping processes. Busy or uninspectable
old directories remain; later installs clean them. Prefix locks serialize installers;
interrupted staging is reclaimed on later runs. Existing v0.3.0 CLIs embed old installer
text, so their first upgrade may still use a legacy directory. The updated CLI's next
normal update migrates it. Server tar entry lists remain compatible with that embedded
allowlist; complete source retrieval directions ship in LICENSES.md, while the full
source document/manifest are separate release assets and native npm files.

## Verification boundaries

Local validation includes Rust workspace tests and Clippy, TypeScript checks,
Node native/filter/playback tests, clean packed-package loading, server/hybrid
integration, source vendoring and offline compilation, static binary import audits,
16-package packing with binary fixtures, native installed-path startup and an existing
WebSocket surviving two updates, upload/network/npm retry simulations,
installer/update tests, Actionlint and a full docs export/link check. Windows-only
installed CLI execution runs in Windows CI, not on this Mac.

Hosted cold/warm total duration, Windows vcpkg cache restore/save costs, target build
and artifact transfer timings after this change remain unmeasured until PR CI runs.
Do not extrapolate this Mac's times to macOS x64 or Windows. No normal CI cache is
deleted to manufacture a cold run. Source completeness on every target and the
first actual source publication still require hosted release validation; no live
publish is authorized as a test. Technical checks do not establish legal compliance;
redistributors must assess additional User Product / installation-information and
platform runtime notice obligations.
