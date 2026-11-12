# Remedia documentation site

This Next.js and Fumadocs app contains Remedia's user-facing documentation. Content lives under
`content/docs/`, grouped by workspace module; `meta.json` files define navigation. The homepage
is `src/app/(home)/page.tsx`. Shared layout and content configuration live in `src/lib/`.

## Local development

From the repository root:

```sh
pnpm --filter docs dev
pnpm --filter docs build
python3 -m http.server 3000 --directory apps/docs/out
```

The build uses Next.js `output: 'export'` and writes a self-contained static site to `apps/docs/out`.
There is no production Node.js server or `next start` command. Search runs in the browser against
the exported `/api/search` index. Social previews and Markdown endpoints are generated at build time.

## Generated API reference

TypeScript and Rust API pages are generated from source into the ignored `.generated/api` directory
by `pnpm docs:api` (run from the repository root, and automatically by `pnpm --filter docs dev` and
`pnpm --filter docs build`). They are merged into the Fumadocs source in `src/lib/source.ts` and
rendered by `src/components/api`. See the [generation guide](content/docs/architecture/api-reference.mdx).
The Rust half needs the pinned nightly toolchain (installed automatically by `rustup`) and the native
FFmpeg build dependencies.

## GitHub Pages deployment

`.github/workflows/deploy-docs.yaml` builds and deploys documentation changes pushed to `main`.
It also supports manual dispatch on `main`. In repository **Settings → Pages**, select
**GitHub Actions** as the build and deployment source. The workflow uploads `apps/docs/out` and
deploys it through the `github-pages` environment. The generated API reference documents the Rust crates, so the workflow installs the same FFmpeg build dependencies as CI.

The workflow reads the site's URL and base path from `actions/configure-pages`, supporting both
project sites such as `https://twlite.github.io/remedia` and custom-domain sites at `/`.
For other static hosts, configure these build-time variables:

- `NEXT_PUBLIC_SITE_URL`: the full public site URL, including any project path. Used for social image URLs.
- `NEXT_PUBLIC_BASE_PATH`: the project path, such as `/remedia`, or an empty string for the domain root.

Pages have trailing slashes so nested documentation URLs work on static hosts. Request-time
Markdown negotiation and `.md` rewrites are unavailable without a server. Use the exported
`/llms.txt`, `/llms-full.txt`, and `/llms.mdx/docs/<slug>/content.md` files instead; the docs copy
buttons use those direct files.

## Social previews

The homepage and documentation pages generate branded 1200 × 630 PNG images at build time.
No external image service is required. `NEXT_PUBLIC_SITE_URL` must include the project path when
one is configured, so both homepage and per-page Open Graph and Twitter images resolve correctly.

## Installer downloads

The canonical installer scripts live at `scripts/install.sh` and `scripts/install.ps1`
in the repository root. The docs build copies them to the ignored
`apps/docs/public/install.sh` and `apps/docs/public/install.ps1` before static export.
GitHub Pages serves them at `https://remedia.twlite.dev/install.sh` and
`https://remedia.twlite.dev/install.ps1`.

Edit only the canonical scripts. Changes to either trigger a documentation deployment.
