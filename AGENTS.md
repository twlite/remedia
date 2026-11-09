# Agent instructions for Remedia

These instructions apply to the entire repository. Follow any more specific `AGENTS.md` files in the directories you modify, provided they do not conflict with the project's core architectural decisions.

## Project overview

Remedia is a Rust-first, embeddable audio engine. It provides native media decoding and playback facilities that can be used directly from Node.js through N-API or controlled through a standalone remote server.

Keep the engine independent of music providers and Discord-specific concerns. The source of audio is the client's responsibility. Remedia is not a source extractor, music search service, or Lavalink-compatible API.

Planned component boundaries (verify what is actually present before modifying code):

- `remedia-core`: Media input, demuxing, decoding, resampling, playback state, filtering, and generic audio output. It must not depend on Discord, Node.js, or WebSocket transport.
- `remedia-discord`: Discord voice transport and associated protocol requirements. It consumes the engine's media output rather than owning decoding.
- `remedia-server`: Standalone server exposing Remedia over WebSocket RPC. Remote input initially supports HTTP(S) URLs and files accessible to the server.
- `remedia-node`: N-API bindings providing direct, in-process access to the Rust engine, without running `remedia-server` as an intermediary. Client-generated streams and buffers belong here initially.
- `apps/docs`: User-facing documentation built with Fumadocs.

These describe intended responsibilities, not a claim that every component or feature already exists. Do not create empty crates, packages, or APIs to satisfy a diagram.

## Before making changes

1. Inspect the relevant code and existing public interfaces. Read `README.md`, `CONTRIBUTING.md`, workspace manifests, and the documentation pages for the area you are changing.
2. Check the actual Cargo and pnpm workspace structure, dependency versions, scripts, tests, and CI configuration. Do not assume a package, command, or feature exists.
3. Preserve existing working architecture and conventions. Prefer a focused change over a broad rewrite.
4. For behavior or API changes, identify the tests **and Fumadocs pages** that need to change before implementing.
5. If a requirement is ambiguous, favor the smallest design consistent with existing code. State significant assumptions in the final summary.

## Engineering rules

- Use native FFmpeg libraries through Rust bindings. Do not spawn `ffmpeg` or `ffprobe` subprocesses for media processing.
- Keep generic media handling in `remedia-core` and transport-specific logic outside it.
- Core inputs include explicit local files and HTTP(S) URLs, plus owned bytes and bounded byte streams for concrete in-process producers. Treat a remote file path as a path on the server's filesystem, not the client's machine.
- Do not add arbitrary client-stream uploads over WebSocket unless explicitly requested. The initial remote protocol should not transfer client-generated media bytes; N-API is the intended local ingestion path.
- Keep sessions independent. Avoid global mutable playback state, unbounded queues, and blocking operations on async executors.
- Make cancellation, shutdown, error propagation, ownership, and resource cleanup explicit. Apply bounded buffering and backpressure where producers and consumers have independent lifecycles.
- Avoid unnecessary cloning or copying of large audio buffers. Optimize after correctness, and measure performance claims.
- Keep unsafe Rust and FFI interactions narrowly scoped. Explain the safety conditions and do not assume an FFmpeg object is thread-safe without evidence.
- Do not invent abstractions for hypothetical inputs, outputs, or plugins. Add a shared abstraction when at least one concrete use case requires it.
- Preserve compatibility for existing public APIs unless the task explicitly calls for a breaking change. Explain intentional breaking changes.
- Prefer typed errors and meaningful failure modes over panics. Never silently discard failures that should reach callers.
- Keep changes scoped. Do not add unrelated dependencies, refactor unrelated modules, or replace existing tooling without a concrete reason.

## Rust and TypeScript conventions

- Use the repository's pinned toolchains and dependency versions where available.
- Format Rust with rustfmt and check it with Clippy. Follow existing workspace lint rules; do not silence warnings globally.
- Use pnpm for JavaScript and TypeScript workspaces, Oxlint for linting, Oxfmt for formatting, and the project's shared TypeScript configurations.
- Do not introduce ESLint, Prettier, a second package manager, or a new task orchestrator without an explicit requirement.
- Give code breathing room: use blank lines between declarations, methods, test cases, and logical steps within functions. Keep closely related statements grouped, and keep documentation comments attached to the declarations they describe. Avoid dense blocks of code with no visual separation; preserve this spacing when formatting.
- Keep TypeScript interfaces strict and avoid untyped escape hatches where practical.
- Expose a small public API. Document important invariants, units, lifetimes, ownership, input formats, and failure cases.
- Never change generated bindings or generated files manually when a source-of-truth generator exists.
- Document every public TypeScript interface, type, class, property, accessor, constructor, function,
  and method with meaningful JSDoc suitable for generated API documentation. Include a `@param`
  tag for every declared parameter (including optional and rest parameters), using its exact name
  and explaining its meaning, units, constraints, and defaults where applicable. Include `@returns`
  for returned values/promises, relevant `@throws`/rejection behavior, and generic type parameter
  documentation. Describe lifecycle, ownership, nullability, and embedded/remote limitations where
  they affect callers. Keep these comments aligned with the implementation and preserve them in
  emitted declarations. Document generated APIs in their source of truth, never by hand-editing
  generated bindings. JSDoc supplements the required `apps/docs` guides and references.

### Node package source layout

Keep `packages/remedia-node/src/` for Rust source only, including the N-API crate modules. Keep
TypeScript source only in `packages/remedia-node/lib/`; `packages/remedia-node/dist/` is compiled
TypeScript output. N-API generated loader and declaration files (`native.js`, `native.d.ts`, and
platform binaries) stay at the package root and must be regenerated from Rust source rather than
edited manually. Put Node tests in `packages/remedia-node/tests/` as TypeScript Vitest files, grouped
by behavior. Do not mix Rust and TypeScript sources in the same `src/` directory.

## Documentation is part of implementation

**All new or changed user-facing features and public APIs must be documented in `apps/docs` in the same change.** Documentation is a completion requirement, not optional follow-up work.

This includes changes to:

- Public Rust APIs, exported types, methods, events, errors, and behavior.
- N-API/Node.js APIs, input types, options, lifecycle, and examples.
- WebSocket RPC commands, request/response formats, notifications, errors, and protocol behavior.
- Playback controls, seeking semantics, timestamps, stream behavior, filters, and supported input/output formats.
- Installation requirements, native dependencies, configuration, environment variables, and supported platforms.
- Limits, compatibility changes, deprecations, and operational constraints that affect consumers.

### How to update the Fumadocs site

1. Inspect `apps/docs` before editing. Find the actual Fumadocs content collection from `source.config.ts` or the equivalent source configuration. The content is commonly under `apps/docs/content/docs`, but follow the configured path rather than assuming it.
2. Find the existing page that covers the changed feature or API and update it. Create a new Markdown/MDX page if the topic does not belong in an existing page.
3. Follow the site's existing frontmatter schema, MDX component conventions, information architecture, filenames, and writing style. Include a clear title and description where required.
4. Update the relevant `meta.json` files or other navigation configuration when adding, removing, or moving pages. Make sure pages are discoverable from the sidebar and related guides.
5. Use concrete examples that match the code implemented in the same change. Verify import paths, API names, argument types, defaults, and expected results. Never describe planned functionality as available.
6. For an API addition or change, describe what it does, its signature or wire format, parameters, return values, errors, important defaults, lifecycle or resource ownership, and at least one realistic usage example where appropriate.
7. For behavior changes, explain what changed from the user's point of view, including limitations and compatibility notes when relevant.
8. Keep conceptual guides and API references distinct when that makes navigation clearer. Link related pages instead of duplicating whole explanations.
9. Run the docs project's supported validation, type-checking, and build commands where feasible. Fix broken frontmatter, MDX compilation, navigation, imports, and links introduced by the change.

Do not substitute Rustdoc comments, a root `docs/` file, a PR description, or a README update for user-facing Fumadocs documentation. Those may supplement `apps/docs`, not replace it.

A purely internal refactor with no observable change normally does not require a docs edit. If docs are unchanged, explicitly say why in the final summary. Do not generate meaningless docs edits just to satisfy a checklist.

## Tests and verification

- Add or update tests when changing behavior. Prefer deterministic tests that run locally without third-party services.
- Use small, legally redistributable or generated audio fixtures. Cover malformed inputs, cleanup, cancellation, end-of-stream behavior, timing and format conversion where relevant.
- For concurrency-related changes, check independent sessions, bounded memory use, shutdown, and race-sensitive behavior.
- Do not make normal CI depend on public music services, expiring URLs, or external network reliability.
- Run the relevant checks for the packages or crates changed. Use the repository's actual commands; common checks include:

  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
  - `pnpm lint`
  - `pnpm format:check`
  - `pnpm typecheck`

- Check the docs app with its existing build or validation script whenever documentation changes.
- Do not claim a check passed if it did not run. Report environment blockers, such as missing native FFmpeg headers or libraries, precisely.

## Completion criteria

A change is complete only when its implementation, relevant tests, public API comments, and affected `apps/docs` pages agree.

In the final summary, report what changed, the checks performed and their results, relevant documentation pages added or updated, any compatibility implications, and remaining limitations. Keep the summary factual and concise.
