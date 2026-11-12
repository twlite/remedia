import { spawn } from 'node:child_process';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

import {
  RAW_DIR,
  REPO_ROOT,
  RUSTDOC_FORMAT_VERSION,
  RUSTDOC_TARGET_DIR,
  RUSTDOC_TOOLCHAIN,
  RUST_CRATES,
  rustLibName,
  type RustCrateConfig,
} from './config.ts';
import type { RustdocCrate } from './rustdoc-types.ts';

/** Raised when the pinned toolchain or rustdoc JSON format is unavailable or different. */
export class RustdocFormatError extends Error {}

/** Overrides used by tests to document fixture crates instead of the repository workspace. */
export interface RustdocRunOptions {
  /** Workspace manifest to document; defaults to the repository workspace with `--locked`. */
  manifestPath?: string;
  targetDir?: string;
  rawDir?: string;
}

/** Result of one cargo invocation covering all configured crates. */
export interface RustdocResult {
  crate: RustCrateConfig;
  /** Absolute path of the copied rustdoc JSON. */
  file: string;
  warnings: string[];
}

/**
 * Run a process and collect output.
 *
 * @param command Executable name.
 * @param args Arguments, passed without a shell.
 * @param env Extra environment variables.
 * @returns Exit code and captured streams.
 */
function run(
  command: string,
  args: string[],
  env: Record<string, string> = {},
): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      cwd: REPO_ROOT,
      env: { ...process.env, ...env },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';

    child.stdout.on('data', (chunk: Buffer) => (stdout += chunk.toString()));
    child.stderr.on('data', (chunk: Buffer) => (stderr += chunk.toString()));
    child.once('error', reject);
    child.once('close', (code) => resolve({ code: code ?? 1, stdout, stderr }));
  });
}

/**
 * Make sure the pinned nightly toolchain is installed. The repository's stable toolchain is never changed.
 *
 * @throws {RustdocFormatError} When rustup is unavailable or installation fails.
 */
export async function ensureRustdocToolchain(): Promise<void> {
  const probe = await run('rustc', [`+${RUSTDOC_TOOLCHAIN}`, '--version']).catch(() => undefined);

  if (probe?.code === 0) return;

  console.log(`Installing ${RUSTDOC_TOOLCHAIN} for rustdoc JSON (documentation only)...`);

  const install = await run('rustup', [
    'toolchain',
    'install',
    RUSTDOC_TOOLCHAIN,
    '--profile',
    'minimal',
    '--no-self-update',
  ]).catch((error: unknown) => {
    throw new RustdocFormatError(
      `rustup is required to install ${RUSTDOC_TOOLCHAIN}: ${error instanceof Error ? error.message : String(error)}`,
    );
  });

  if (install.code !== 0) {
    // Parallel callers (for example concurrent test files) can race on the installation; accept it if another won.
    const retry = await run('rustc', [`+${RUSTDOC_TOOLCHAIN}`, '--version']).catch(() => undefined);

    if (retry?.code === 0) return;

    throw new RustdocFormatError(`Could not install ${RUSTDOC_TOOLCHAIN}:\n${install.stderr}`);
  }
}

/**
 * Generate rustdoc JSON for every configured library crate.
 *
 * The documentation toolchain is selected with `cargo +<toolchain>`, so builds, tests, and Clippy keep
 * using `rust-toolchain.toml`. Private items are not documented; `#[doc(hidden)]` items are omitted by rustdoc.
 *
 * @param crates Library crates to document.
 * @returns One JSON copy per crate plus rustdoc warnings.
 */
export async function runRustdoc(
  crates: RustCrateConfig[] = RUST_CRATES,
  options: RustdocRunOptions = {},
): Promise<RustdocResult[]> {
  const rawDir = options.rawDir ?? RAW_DIR;
  const targetDir = options.targetDir ?? RUSTDOC_TARGET_DIR;

  await ensureRustdocToolchain();
  await mkdir(rawDir, { recursive: true });

  const packageArgs = crates.flatMap((crate) => ['-p', crate.name]);
  const result = await run(
    'cargo',
    [
      `+${RUSTDOC_TOOLCHAIN}`,
      'doc',
      '--no-deps',
      '--lib',
      ...(options.manifestPath ? ['--manifest-path', options.manifestPath] : ['--locked']),
      '--target-dir',
      targetDir,
      ...packageArgs,
    ],
    {
      RUSTDOCFLAGS: [process.env.RUSTDOCFLAGS ?? '', '-Z unstable-options --output-format json']
        .join(' ')
        .trim(),
    },
  );

  if (result.code !== 0) {
    throw new RustdocFormatError(
      `cargo doc failed with exit code ${result.code}:\n${result.stderr}`,
    );
  }

  const warnings = result.stderr
    .split('\n')
    .filter((line) => line.startsWith('warning:') && !line.includes('generated'));
  const results: RustdocResult[] = [];

  for (const crate of crates) {
    const source = join(targetDir, 'doc', `${rustLibName(crate.name)}.json`);
    const text = await readFile(source, 'utf8');
    const json = JSON.parse(text) as RustdocCrate;

    assertRustdocFormat(json, crate.name);

    const file = join(rawDir, `rustdoc-${crate.slug}.json`);

    await writeFile(file, text);
    results.push({ crate, file, warnings });
  }

  return results;
}

/**
 * Validate the rustdoc JSON `format_version`. rustdoc's format is unstable, so the normalizer only
 * accepts the exact version of the pinned toolchain.
 *
 * @param json Parsed rustdoc JSON.
 * @param crate Crate name for the message.
 * @throws {RustdocFormatError} When the format version differs.
 */
export function assertRustdocFormat(
  json: Pick<RustdocCrate, 'format_version'>,
  crate: string,
): void {
  if (json.format_version !== RUSTDOC_FORMAT_VERSION) {
    throw new RustdocFormatError(
      `${crate}: rustdoc JSON format ${json.format_version} was produced, but ${RUSTDOC_TOOLCHAIN} is pinned to format ${RUSTDOC_FORMAT_VERSION}. Update scripts/docs/config.ts and rustdoc-types.ts together.`,
    );
  }
}
