# Contributing to Remedia

Remedia is under development. Contributions that improve the existing code or make the project
easier to build and maintain are welcome. Playback, transport, server, and binding features should
follow the intended boundaries in [`docs/architecture.md`](docs/architecture.md).

## Native build prerequisites

Use the pinned static recipes described below. Install a C compiler, make, NASM,
pkg-config, and libclang. Runtime users do not need these tools.

## Set up the repository

```sh
git clone https://github.com/twlite/remedia.git
cd remedia
corepack enable
pnpm --version
pnpm install --frozen-lockfile
```

The pnpm version should be `12.16.1`. If Corepack is unavailable, install that version with
`npm install --global pnpm@12.16.1`.

Rustup reads `rust-toolchain.toml` automatically. To install the pinned toolchain and its requested
components explicitly, run:

```sh
rustup toolchain install 1.99.1 --component rustfmt --component clippy
```

## Development commands

Run these from the repository root:

```sh
pnpm lint             # lint JavaScript and TypeScript with Oxlint
pnpm lint:fix         # apply Oxlint's safe automatic fixes
pnpm format           # format supported files with Oxfmt
pnpm format:check     # check formatting without changing files
pnpm typecheck        # run each workspace package's typecheck command
pnpm check            # run the full local validation sequence
pnpm --filter discord-app test
pnpm --filter discord-app start  # manual Discord integration; requires apps/discord/.env
pnpm --filter @remedia/client test
pnpm --filter @remedia/client build

cargo fmt --all
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p remedia-server --all-targets
pnpm --filter docs build
```

The docs app can be run with `pnpm --filter docs dev` and built with
`pnpm --filter docs build`. Both first generate the TypeScript and Rust API reference with
`pnpm docs:api` (TypeDoc and rustdoc JSON, using a pinned nightly toolchain for documentation only,
and the native FFmpeg dependencies); `pnpm docs:test` tests the generator. See
[Generated API reference](apps/docs/content/docs/architecture/api-reference.mdx). `pnpm --filter @remedia/node test` builds the debug N-API addon and runs
the TypeScript integration tests with Vitest. To separate compilation from execution, run
`pnpm --filter @remedia/node build:native` (release addon),
`pnpm --filter @remedia/node build:types`, then
`pnpm --filter @remedia/node test:bindings`. `test:bindings` never compiles native code and is the
CI entry point after downloading a target artifact. Add tests under
`packages/remedia-node/tests/` and split them by behavior; `typecheck` checks both library and test
TypeScript. The package keeps Rust code in `src/`, TypeScript library code in `lib/`, and generated
TypeScript output in `dist/`.

#### FFmpeg loading in `@remedia/node`

The engine addon statically links the pinned FFmpeg build. No bootstrap discovery
or macOS install-name rewriting is needed. The dependency audit rejects external
media libraries. Build scripts regenerate native.js and native.d.ts from Rust.

## CI change selection

Every push to main and every relevant pull request runs release/change detection.
Feature branches run through pull requests, avoiding duplicate push and PR matrices. The remaining checks run only for workspace code, application documentation content,
build and release configuration, distributed license files, or a detected release. Ordinary
README-only changes, contributor guides, and issue/PR templates skip those checks. Pull requests
retain workflow path filters. Manual workflow dispatch runs all checks.

Changes confined to `apps/docs`, `apps/discord`, shared npm lockfiles, or license texts run the
JavaScript, packaging-test, and documentation checks without native build matrices or release packing. Rust documentation generation
still requires cached static FFmpeg headers. Rust crates, the Node and client packages
(which exercise native integration), native build recipes, workflow scripts, and shared build
configuration enable native checks. TypeScript-only integration changes omit additional
cross builds; low-level and release changes retain every target. Detected release commits always enable all checks.
Push comparisons cover the entire push; pull requests compare from the merge base. New feature
branches compare with the default branch. Unavailable history falls back to full validation.
Run `pnpm test:ci` to check this selection logic locally.

## Automated releases

Stable and prerelease releases are created by pushing a commit to main whose entire subject is
'chore: release x.y.z' or 'chore: release x.y.z-tag.n'. Stable versions publish with npm's
'latest' dist-tag. Prerelease versions use their lowercase identifier as the dist-tag, such as
'canary' in '1.3.0-canary.5'. Ordinary pushes and pull requests never publish. Both stable and
prerelease release commits bypass change selection: empty commits and README-only release
commits on main start the full release pipeline.

Only collaborators with current repository **write**, **maintain**, or **admin** access may trigger
release actions. CI checks both the original workflow actor and the actor initiating a rerun using
GitHub's repository permission API. Read/triage access, missing identities, and failed permission
lookups block release actions. Commit author names and emails are not used for authorization.
Each tag, draft, upload, npm publication, and GitHub publication stage rechecks permissions before
making changes, including when an individual job is rerun.

Manual workflow dispatch on `main` can also release the selected commit when its subject matches
the same release format. Dispatches on other branches or ordinary commits run checks only.

The workflow overrides the @remedia/node, @remedia/client, and @remedia/plugin-host npm versions in each CI checkout
before installing, building, or packing. It does not commit those overrides or alter Rust crate
versions. @napi-rs/cli creates platform packages and synchronizes their versions; pnpm pack
resolves the client's workspace:* peer dependency in the packed manifest. All 13 configured
addon targets are packed and published. Exact package versions already on npm are skipped, and
platform packages publish before @remedia/node, followed by @remedia/plugin-host and @remedia/client.

A detected release first creates (or reuses) a GitHub draft and verifies that its immutable tag
points to the release commit. After checks, native builds, and packaging pass, all 13 `.node`
addons, eight server executables, eight server archives, and license/source-distribution files are
uploaded to that draft. Upload completeness, sizes, and SHA-256 digests are verified
before npm publication starts. The final job waits for every npm package publish to succeed,
rechecks the assets, and only then makes the GitHub release public. Prerelease versions remain
marked as prereleases and are not promoted to GitHub's latest stable release.

Failures before finalization leave the draft and tag in place for inspection or a rerun. Matching draft
assets are preserved on retries; mismatched completed assets block publication. A release that is already public is never changed back to a
draft or overwritten by a fresh full run; retrying only the final publication job is harmless.
The npm registry and GitHub are separate systems: npm versions published before a later failure
remain published. The existing exact-version checks allow a failed npm stage to resume.

For token-free releases, configure npm Trusted Publishing for each package the workflow publishes:

- @remedia/node-linux-x64-gnu
- @remedia/node-linux-arm64-gnu
- @remedia/node-darwin-x64
- @remedia/node-darwin-arm64
- @remedia/node-win32-x64-msvc
- @remedia/node
- @remedia/client
- @remedia/plugin-host

For every package, choose GitHub Actions with organization/user twlite, repository remedia, and
workflow filename ci.yaml. Leave the environment unset and allow direct publishing. The workflow uses a GitHub-hosted Node 24 runner and npm 11.21.0; only its publish job receives
id-token: write, also used for provenance when publishing with a token. See npm's
[trusted publisher setup guide](https://docs.npmjs.com/trusted-publishers/).
`cargo test -p remedia-core` runs deterministic file, in-memory, streaming, and local-HTTP decoder tests. The
core tests cover decoder conversion, seeking, playback lifecycle, native FFmpeg filtergraph processing,
master gain, Opus framing, backpressure, and stop cleanup. `remedia-discord` tests parsing, fail-closed DAVE behavior, RTP, transport encryption,
UDP discovery, pacing, and bounded queue cancellation without Discord credentials. The
examples can be run with `cargo run -p remedia-core --example decode -- file <path>`,
`cargo run -p remedia-core --example decode -- url <http(s)-url>`, or
`cargo run -p remedia-core --example playback -- file <path>`.
The optional manual voice sender example is `cargo run -p remedia-discord --example play_file`; it
requires externally obtained Discord voice session information and a reachable voice endpoint.
The standalone server reads `remedia.config.json` from the current directory or a `--config PATH`
override. Use `crates/remedia-server/remedia.config.example.json` as the schema example. Its
`remote_discord` example requires an already running server and fresh voice credentials obtained by
an external Discord Gateway client; normal tests do not require Discord credentials.

## Where code belongs

- Add a Rust crate under `crates/` when it has an independently useful boundary in the engine,
  transport, or server. `remedia-core` owns source opening, decoding, resampling, playback sessions,
  generic PCM output, and transport-independent Opus encoding. `remedia-discord` owns Discord voice
  connections, DAVE, RTP, encryption, UDP, and packet scheduling. `remedia-server` owns authenticated
  remote sessions and JSON RPC while reusing those crates. Add crates to the Cargo workspace only
  when they exist and are needed.
- Add a JavaScript or TypeScript package under `packages/` when it has a distinct package boundary.
  Keep an N-API crate alongside its owning Node package: `packages/remedia-node` is included in the
  root Cargo workspace and calls the Remedia engine in process. Keep its Rust files in `src/` and
  TypeScript files in `lib/`; do not mix languages in `src/`.
- Keep documentation tooling and site code under `apps/docs/`.
- Prefer the root shared TypeScript configuration. Add a package-level override only for a real
  requirement, such as NodeNext resolution for a Node library or bundler resolution for a frontend.
- Keep generated native binaries out of commits. Commit generated JS/TypeScript declarations or
  other source artifacts when the package's build and distribution workflow requires them.

Changes to exported Rust APIs, the N-API surface, or the remote protocol need a clear
compatibility note in the pull request. Do not assume the embedded and remote interfaces have the
same input constraints.

## Issues and pull requests

Use the bug report form for reproducible defects and the feature request form for proposed work.
Include operating system and architecture, Rust and Node.js versions where relevant, input media
details, reproduction steps, and useful logs. Remove credentials, private URLs, and other secrets
from logs before sharing them.

Pull requests should explain the change and its scope, link a related issue when available, and
include relevant documentation updates. Run the checks that apply to the change and report any
platform-specific checks that could not be run. Keep pull requests focused and avoid presenting
planned capabilities as already implemented.

## Release license inventory

`LICENSE` applies to Remedia. `THIRD_PARTY_NOTICES.md` highlights the major components, including
bundled libopus and native cryptography. `THIRD_PARTY_LICENSES.txt` preserves the full redistribution
texts from the locked Cargo graph and the client's `ws` runtime dependency.
It includes exact source download links for MPL-covered dependencies. Review and refresh this
inventory when dependencies or native build options change; preserve upstream texts verbatim.
Development tools and docs dependencies are not bundled in the npm packages or server binaries.

Package `prepack` scripts copy the root license and notices into each workspace package. The release
packer also stages them into every generated platform package, checks their exact tarball contents,
and builds per-target server archives with all three files. Run `pnpm test:release`; the tarball tests build the
TypeScript packages. These packaging checks do not replace dependency or legal review.

## Cached native builds and corresponding source

CI caches only compiled dependency prefixes and configuration records, keyed by
actual tool versions, target and native recipe inputs. Windows vcpkg uses its pinned
overlay in classic mode, avoiding an unnecessary full-history clone. Cache hits
still run configuration, ABI, license and linked dependency checks. The release
server and addon share an explicit target directory. Tests use the test profile;
PR artifacts use the optimized no-LTO `ci` profile; releases retain fat LTO.
See `.github/CI_AUDIT.md` for measured tradeoffs and unmeasured limits.

`SOURCE_DISTRIBUTION.md` explains exact-version source retrieval and rebuilding.
Dependency snapshots contain locked Cargo vendor sources and hash-verified native
source archives, reused across releases. Small release-specific source/configuration
archives become public before npm publishes. `source-manifest.json` records their
identities, hashes and URLs. Never delete or replace a source asset while associated
binaries are distributed. v0.3.0's assets are unchanged.

Validate a changed source set locally without publishing:

```sh
node .github/scripts/corresponding-source.mjs snapshot /tmp/remedia-source-snapshot
node .github/scripts/update-third-party-notices.mjs --check
pnpm test:release
```

Use the resulting vendor and native directories for offline rebuilding as described
in `SOURCE_DISTRIBUTION.md`. CI runs stronger source validation when source inputs
change. Custom toolchain distributions and User Products may require additional
notice/installation-information review; technical audits do not decide compliance.

The installers use immutable `libexec/vVERSION` pairs and atomic current/previous
pointers. Layout migration and cleanup never stop running processes. Tests cover
legacy tmp/GUID paths, stale stages, pointer failures, cached rollback and process
retention; native CI additionally launches the installed runtime from its final
path. See the [installation guide](apps/docs/content/docs/server/installation.mdx).
