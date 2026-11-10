import {
  closeSync,
  existsSync,
  openSync,
  readFileSync,
  readSync,
  readdirSync,
  realpathSync,
} from 'node:fs';
import { homedir } from 'node:os';
import { delimiter, dirname, isAbsolute, join, resolve } from 'node:path';

/**
 * Where the loaded FFmpeg installation came from.
 *
 * - `explicit`: the `REMEDIA_FFMPEG_DIR` environment variable.
 * - `system`: an installation the platform's library search finds (package manager, Homebrew,
 *   `LD_LIBRARY_PATH`, `DYLD_LIBRARY_PATH`, `PATH`, ...).
 * - `managed`: the Remedia installer's `~/.remedia/ffmpeg9` prefix.
 * - `unmanaged`: the platform has no bootstrap addon, so the operating system's loader resolved
 *   FFmpeg on its own and Remedia cannot report which installation it chose.
 */
export type FfmpegSource = 'explicit' | 'system' | 'managed' | 'unmanaged';

/**
 * Why an FFmpeg installation candidate was rejected, where that can be determined.
 *
 * - `not-found`: the directory does not exist.
 * - `missing-libraries`: required FFmpeg libraries are absent from the directory.
 * - `abi-mismatch`: libraries exist but have a different ABI major than FFmpeg 9 requires.
 * - `architecture-mismatch`: the libraries were built for a different CPU architecture.
 * - `unresolved-dependency`: a library or one of its own dependencies could not be resolved.
 * - `mixed-installations`: the five libraries did not all come from one directory.
 * - `load-failed`: the loader rejected a library for another reason.
 */
export type FfmpegFailureReason =
  | 'not-found'
  | 'missing-libraries'
  | 'abi-mismatch'
  | 'architecture-mismatch'
  | 'unresolved-dependency'
  | 'mixed-installations'
  | 'load-failed';

/** One FFmpeg shared library loaded by Remedia. */
export interface FfmpegLibraryInfo {
  /** FFmpeg library name without the `lib` prefix, for example `avcodec`. */
  readonly name: string;
  /** Absolute path of the loaded file as reported by the operating system. */
  readonly path: string;
  /** Library version reported by the library itself, as `major.minor.micro`. */
  readonly version: string;
  /** ABI major version, which must match the FFmpeg 9 requirement for `name`. */
  readonly major: number;
}

/** What discovery concluded about one FFmpeg installation candidate. */
export interface FfmpegCandidateReport {
  /** Which discovery step produced this candidate. */
  readonly source: Exclude<FfmpegSource, 'unmanaged'>;
  /** Directory that was inspected, or `null` when the step looked at several directories. */
  readonly directory: string | null;
  /** Whether this candidate was loaded (`selected`) or not. */
  readonly outcome: 'selected' | 'rejected';
  /** Classified rejection cause when `outcome` is `rejected` and the cause is known. */
  readonly reason?: FfmpegFailureReason;
  /** Human-readable explanation, including the loader's own message where available. */
  readonly detail?: string;
}

/** The FFmpeg runtime that `@remedia/node` loaded. */
export interface FfmpegRuntimeInfo {
  /** How the installation was found. */
  readonly source: FfmpegSource;
  /** Directory containing the loaded libraries, or `null` when `source` is `unmanaged`. */
  readonly directory: string | null;
  /** The five loaded libraries in load order; empty when `source` is `unmanaged`. */
  readonly libraries: readonly FfmpegLibraryInfo[];
  /** Every candidate discovery examined, in order, including the selected one. */
  readonly considered: readonly FfmpegCandidateReport[];
}

/** Command that installs FFmpeg 9 on Linux and macOS with the Remedia installer. */
export const UNIX_INSTALL_COMMAND =
  'curl -fsSL https://remedia.twlite.dev/install.sh | sh -s -- --ffmpeg-only';

/** Command that installs FFmpeg 9 on Windows PowerShell with the Remedia installer. */
export const WINDOWS_INSTALL_COMMAND =
  '& ([scriptblock]::Create((irm https://remedia.twlite.dev/install.ps1))) -FFmpegOnly';

/**
 * Raised when no compatible FFmpeg 9 installation could be loaded, or when an explicit
 * `REMEDIA_FFMPEG_DIR` override is unusable. The message lists every path considered and how to
 * install or point to FFmpeg.
 */
export class FfmpegLoadError extends Error {
  /** Every candidate that was examined, in order. */
  readonly candidates: readonly FfmpegCandidateReport[];

  /**
   * @param candidates - Reports for each installation candidate that was examined.
   * @param explicitOverride - Whether `REMEDIA_FFMPEG_DIR` was set, which disables fallbacks.
   * @param platform - Platform used to choose the install command shown in the message.
   */
  constructor(
    candidates: readonly FfmpegCandidateReport[],
    explicitOverride: boolean,
    platform: NodeJS.Platform = process.platform,
  ) {
    super(describeFailure(candidates, explicitOverride, platform));
    this.name = 'FfmpegLoadError';
    this.candidates = candidates;
  }
}

/** FFmpeg 9 shared libraries in dependency order (leaf libraries first) with their ABI majors. */
export const FFMPEG_LIBRARIES = [
  { name: 'avutil', major: 61 },
  { name: 'swresample', major: 7 },
  { name: 'avcodec', major: 63 },
  { name: 'avformat', major: 63 },
  { name: 'avfilter', major: 12 },
] as const;

type LibraryName = (typeof FFMPEG_LIBRARIES)[number]['name'];

/**
 * Result of one native `loadLibrary` call.
 *
 * @internal
 */
export interface BootstrapLoadResult {
  ok: boolean;
  path?: string | null;
  version?: number | null;
  code?: number | null;
  message?: string | null;
}

/**
 * The FFmpeg-free native bootstrap addon.
 *
 * @internal
 */
export interface BootstrapBinding {
  bootstrapVersion(): number;
  loadLibrary(target: string, versionSymbol?: string | null): BootstrapLoadResult;
  commitLoaded(): void;
  rollbackLoaded(): void;
}

/**
 * Process facts discovery depends on, injectable so every platform can be tested anywhere.
 *
 * @internal
 */
export interface DiscoveryEnvironment {
  platform: NodeJS.Platform;
  arch: string;
  env: NodeJS.ProcessEnv;
  homeDirectory: string;
  execPath: string;
  /** Replaces the platform's default system library directories; used by tests. */
  systemDirectories?: readonly string[];
}

/**
 * The current process as a {@link DiscoveryEnvironment}.
 *
 * @returns Facts read from `process` and the operating system.
 * @internal
 */
export function currentEnvironment(): DiscoveryEnvironment {
  return {
    platform: process.platform,
    arch: process.arch,
    env: process.env,
    homeDirectory: homedir(),
    execPath: process.execPath,
  };
}

/**
 * File name of one FFmpeg library at its ABI major on a platform.
 *
 * @param name - FFmpeg library name without the `lib` prefix.
 * @param major - ABI major version.
 * @param platform - Node.js platform string.
 * @returns For example `libavcodec.so.63`, `libavcodec.63.dylib` or `avcodec-63.dll`.
 * @internal
 */
export function libraryFileName(name: string, major: number, platform: NodeJS.Platform): string {
  if (platform === 'win32') return `${name}-${major}.dll`;
  if (platform === 'darwin') return `lib${name}.${major}.dylib`;

  return `lib${name}.so.${major}`;
}

/**
 * Directories of a prefix that may hold the shared libraries, most specific first.
 *
 * @param root - A prefix such as `~/.remedia/ffmpeg9`, or the library directory itself.
 * @param platform - Node.js platform string.
 * @returns Candidate library directories inside `root`.
 * @internal
 */
export function layoutDirectories(root: string, platform: NodeJS.Platform): string[] {
  if (platform === 'win32') return [join(root, 'Library', 'bin'), join(root, 'bin'), root];

  return [join(root, 'lib'), join(root, 'lib64'), root];
}

/**
 * Default location of the installer-managed FFmpeg prefix for the executing user.
 *
 * @param environment - Process facts; only the home directory is used.
 * @returns `<home>/.remedia/ffmpeg9`.
 * @internal
 */
export function managedPrefix(environment: DiscoveryEnvironment): string {
  return join(environment.homeDirectory, '.remedia', 'ffmpeg9');
}

function splitPathList(value: string | undefined): string[] {
  return (value ?? '')
    .split(delimiter)
    .map((entry) => entry.trim().replace(/^"(.*)"$/, '$1'))
    .filter((entry) => entry.length > 0 && isAbsolute(entry));
}

function readLoaderConfiguration(file: string, depth = 0): string[] {
  if (depth > 4) return [];

  let contents: string;

  try {
    contents = readFileSync(file, 'utf8');
  } catch {
    return [];
  }

  const directories: string[] = [];

  for (const rawLine of contents.split('\n')) {
    const line = rawLine.replace(/#.*$/, '').trim();

    if (line.length === 0) continue;

    if (line.startsWith('include ')) {
      const pattern = line.slice('include '.length).trim();
      const directory = dirname(pattern);
      const matcher = new RegExp(
        `^${pattern
          .slice(directory.length + 1)
          .replace(/[.+^${}()|[\]\\]/g, '\\$&')
          .replace(/\*/g, '.*')
          .replace(/\?/g, '.')}$`,
      );

      try {
        for (const entry of readdirSync(directory).sort()) {
          if (matcher.test(entry)) {
            directories.push(...readLoaderConfiguration(join(directory, entry), depth + 1));
          }
        }
      } catch {
        // A missing include directory is normal.
      }
    } else if (isAbsolute(line)) {
      directories.push(line);
    }
  }

  return directories;
}

/**
 * Directories the platform's own library search would consult, for finding an existing
 * system-wide FFmpeg without invoking the loader (which could half-load a broken installation).
 *
 * The current directory is never included, so a planted library there cannot be selected.
 *
 * @param environment - Process facts.
 * @returns De-duplicated absolute directories in search order.
 * @internal
 */
export function systemDirectories(environment: DiscoveryEnvironment): string[] {
  const { platform, arch, env } = environment;
  let directories: string[] = [];

  if (platform === 'win32') {
    directories = [dirname(environment.execPath), ...splitPathList(env.PATH ?? env.Path)];
  } else if (platform === 'darwin') {
    directories = [
      ...splitPathList(env.DYLD_LIBRARY_PATH),
      ...splitPathList(env.DYLD_FALLBACK_LIBRARY_PATH),
      '/opt/homebrew/opt/ffmpeg/lib',
      '/usr/local/opt/ffmpeg/lib',
      '/opt/homebrew/lib',
      '/usr/local/lib',
    ];
  } else if (platform === 'linux') {
    const multiarch =
      arch === 'x64'
        ? 'x86_64-linux-gnu'
        : arch === 'arm64'
          ? 'aarch64-linux-gnu'
          : arch === 'arm'
            ? 'arm-linux-gnueabihf'
            : null;

    directories = [
      ...splitPathList(env.LD_LIBRARY_PATH),
      ...readLoaderConfiguration('/etc/ld.so.conf'),
      '/usr/local/lib',
      ...(multiarch ? [`/usr/lib/${multiarch}`, `/lib/${multiarch}`] : []),
      '/usr/lib64',
      '/usr/lib',
      '/lib64',
      '/lib',
    ];
  } else if (platform === 'freebsd') {
    directories = [...splitPathList(env.LD_LIBRARY_PATH), '/usr/local/lib', '/usr/lib', '/lib'];
  } else {
    directories = splitPathList(env.LD_LIBRARY_PATH);
  }

  return [...new Set(directories.map((directory) => resolve(directory)))];
}

/**
 * Architectures a native binary is built for, read from its file header.
 *
 * @param file - Path to an ELF, Mach-O (thin or universal) or PE file.
 * @returns Node.js `process.arch` names, or `null` when the format is not recognized.
 * @internal
 */
export function binaryArchitectures(file: string): string[] | null {
  const header = Buffer.alloc(65_536);
  let length: number;
  let descriptor: number | null = null;

  try {
    descriptor = openSync(file, 'r');
    length = readSync(descriptor, header, 0, header.length, 0);
  } catch {
    return null;
  } finally {
    if (descriptor !== null) closeSync(descriptor);
  }

  if (length < 20) return null;

  const mapCpu = (table: Record<number, string>, value: number): string[] =>
    table[value] ? [table[value]] : [];

  // ELF
  if (header.readUInt32BE(0) === 0x7f454c46) {
    const little = header[5] === 1;
    const machine = little ? header.readUInt16LE(18) : header.readUInt16BE(18);

    return mapCpu({ 62: 'x64', 183: 'arm64', 3: 'ia32', 40: 'arm' }, machine);
  }

  // Mach-O thin (little endian host order for every supported Apple CPU)
  const magic = header.readUInt32LE(0);

  if (magic === 0xfeedfacf || magic === 0xfeedface) {
    return mapCpu(
      { 0x01000007: 'x64', 0x0100000c: 'arm64', 7: 'ia32', 12: 'arm' },
      header.readUInt32LE(4),
    );
  }

  // Mach-O universal (always big endian)
  if (header.readUInt32BE(0) === 0xcafebabe || header.readUInt32BE(0) === 0xcafebabf) {
    const wide = header.readUInt32BE(0) === 0xcafebabf;
    const count = header.readUInt32BE(4);
    const size = wide ? 32 : 20;

    if (count === 0 || count > 16) return null;

    const architectures: string[] = [];

    for (let index = 0; index < count && 8 + (index + 1) * size <= length; index += 1) {
      architectures.push(
        ...mapCpu(
          { 0x01000007: 'x64', 0x0100000c: 'arm64', 7: 'ia32', 12: 'arm' },
          header.readUInt32BE(8 + index * size),
        ),
      );
    }

    return architectures;
  }

  // PE
  if (header[0] === 0x4d && header[1] === 0x5a && length >= 0x40) {
    const offset = header.readUInt32LE(0x3c);

    if (offset + 6 <= length && header.readUInt32LE(offset) === 0x00004550) {
      return mapCpu(
        { 0x8664: 'x64', 0xaa64: 'arm64', 0x14c: 'ia32', 0x1c4: 'arm' },
        header.readUInt16LE(offset + 4),
      );
    }
  }

  return null;
}

interface Inspection {
  /** Absolute path of each required library, when all five exist. */
  files: { name: LibraryName; major: number; path: string }[] | null;
  missing: string[];
  /** Other ABI majors of the same libraries that are present, for diagnostics. */
  otherMajors: string[];
  found: number;
}

function inspectDirectory(directory: string, platform: NodeJS.Platform): Inspection {
  const missing: string[] = [];
  const otherMajors: string[] = [];
  const files: { name: LibraryName; major: number; path: string }[] = [];
  let entries: string[] = [];

  try {
    entries = readdirSync(directory);
  } catch {
    // Reported as missing libraries below.
  }

  for (const { name, major } of FFMPEG_LIBRARIES) {
    const fileName = libraryFileName(name, major, platform);
    const path = join(directory, fileName);

    if (existsSync(path)) {
      files.push({ name, major, path });
      continue;
    }

    missing.push(fileName);

    const pattern =
      platform === 'win32'
        ? new RegExp(`^${name}-(\\d+)\\.dll$`, 'i')
        : platform === 'darwin'
          ? new RegExp(`^lib${name}\\.(\\d+)\\.dylib$`)
          : new RegExp(`^lib${name}\\.so\\.(\\d+)`);

    for (const entry of entries) {
      const match = pattern.exec(entry);

      if (match) otherMajors.push(`lib${name} ABI ${match[1]} (requires ${major})`);
    }
  }

  return {
    files: missing.length === 0 ? files : null,
    missing,
    otherMajors: [...new Set(otherMajors)],
    found: FFMPEG_LIBRARIES.length - missing.length,
  };
}

function classifyLoaderFailure(result: BootstrapLoadResult): FfmpegFailureReason {
  const message = result.message ?? '';

  // Only Windows reports numeric codes (ERROR_MOD_NOT_FOUND and ERROR_BAD_EXE_FORMAT); Unix
  // loaders report text, matched below.
  if (result.code === 126) return 'unresolved-dependency';
  if (result.code === 193) return 'architecture-mismatch';

  if (
    /wrong ELF class|invalid ELF header|incompatible architecture|not a mach-o|bad image/i.test(
      message,
    )
  ) {
    return 'architecture-mismatch';
  }

  if (
    /cannot open shared object file|library not loaded|image not found|module could not be found/i.test(
      message,
    )
  ) {
    return 'unresolved-dependency';
  }

  return 'load-failed';
}

function stripExtendedPathPrefix(path: string): string {
  return path.startsWith('\\\\?\\') ? path.slice(4) : path;
}

function formatVersion(packed: number): string {
  return `${packed >>> 16}.${(packed >>> 8) & 0xff}.${packed & 0xff}`;
}

interface Rejection {
  reason: FfmpegFailureReason;
  detail: string;
}

type Attempt =
  | { ok: true; libraries: FfmpegLibraryInfo[]; directory: string }
  | ({ ok: false } & Rejection);

/**
 * Load one directory's FFmpeg libraries through the bootstrap, all or nothing.
 *
 * Nothing stays loaded after a failure, so a later candidate can never bind to libraries left over
 * from a rejected one.
 *
 * @param bootstrap - The FFmpeg-free native bootstrap.
 * @param directory - Directory that should contain all five libraries.
 * @param environment - Process facts.
 * @returns The loaded libraries, or the classified rejection.
 * @internal
 */
export function loadDirectory(
  bootstrap: BootstrapBinding,
  directory: string,
  environment: DiscoveryEnvironment,
): Attempt {
  const inspection = inspectDirectory(directory, environment.platform);

  if (!inspection.files) {
    const others =
      inspection.otherMajors.length > 0 ? ` Found: ${inspection.otherMajors.join(', ')}.` : '';

    return {
      ok: false,
      reason: inspection.otherMajors.length > 0 ? 'abi-mismatch' : 'missing-libraries',
      detail: `Missing ${inspection.missing.join(', ')}.${others}`,
    };
  }

  for (const { path } of inspection.files) {
    const architectures = binaryArchitectures(path);

    if (architectures !== null && !architectures.includes(environment.arch)) {
      return {
        ok: false,
        reason: 'architecture-mismatch',
        detail: `${path} is built for ${architectures.join('/') || 'an unknown CPU'}, but this process is ${environment.arch}.`,
      };
    }
  }

  const libraries: FfmpegLibraryInfo[] = [];

  const fail = (rejection: Rejection): Attempt => {
    bootstrap.rollbackLoaded();

    return { ok: false, ...rejection };
  };

  for (const { name, major, path } of inspection.files) {
    const result = bootstrap.loadLibrary(path, `${name}_version`);

    if (!result.ok) {
      return fail({
        reason: classifyLoaderFailure(result),
        detail: `${path}: ${result.message ?? 'the loader rejected the library'}`,
      });
    }

    const packed = result.version ?? 0;

    if (packed >>> 16 !== major) {
      return fail({
        reason: 'abi-mismatch',
        detail: `${path} reports lib${name} ABI ${packed >>> 16}, but ${major} is required.`,
      });
    }

    let loadedPath = stripExtendedPathPrefix(result.path ?? path);

    try {
      loadedPath = realpathSync(loadedPath);
    } catch {
      // Keep the reported path.
    }

    libraries.push({ name, path: loadedPath, version: formatVersion(packed), major });
  }

  const directories = new Set(libraries.map((library) => dirname(library.path)));

  if (directories.size > 1) {
    return fail({
      reason: 'mixed-installations',
      detail: `The FFmpeg libraries resolved to different directories: ${[...directories].join(', ')}.`,
    });
  }

  bootstrap.commitLoaded();

  return { ok: true, libraries, directory: dirname(libraries[0]!.path) };
}

/**
 * Choose and load an FFmpeg 9 installation.
 *
 * Precedence: `REMEDIA_FFMPEG_DIR`, then an installation visible to the system, then the managed
 * prefix `~/.remedia/ffmpeg9`. An explicit override that cannot be loaded is an error; the other
 * steps are never consulted in that case.
 *
 * @param bootstrap - The FFmpeg-free native bootstrap.
 * @param environment - Process facts; defaults to the current process.
 * @returns The selected runtime.
 * @throws {FfmpegLoadError} If no compatible installation can be loaded.
 * @internal
 */
export function selectFfmpeg(
  bootstrap: BootstrapBinding,
  environment: DiscoveryEnvironment = currentEnvironment(),
): FfmpegRuntimeInfo {
  const considered: FfmpegCandidateReport[] = [];

  const attempt = (
    source: FfmpegCandidateReport['source'],
    directory: string,
  ): FfmpegRuntimeInfo | null => {
    const result = loadDirectory(bootstrap, directory, environment);

    if (result.ok) {
      considered.push({ source, directory, outcome: 'selected' });

      return {
        source,
        directory: result.directory,
        libraries: result.libraries,
        considered,
      };
    }

    considered.push({
      source,
      directory,
      outcome: 'rejected',
      reason: result.reason,
      detail: result.detail,
    });

    return null;
  };

  const override = environment.env.REMEDIA_FFMPEG_DIR?.trim();

  if (override) {
    const root = resolve(override);

    if (!existsSync(root)) {
      considered.push({
        source: 'explicit',
        directory: root,
        outcome: 'rejected',
        reason: 'not-found',
        detail: 'REMEDIA_FFMPEG_DIR does not exist.',
      });
      throw new FfmpegLoadError(considered, true, environment.platform);
    }

    // The override may name the installation prefix or the library directory itself. A directory
    // that is already complete wins; otherwise the first layout is used so its report is shown.
    const layouts = layoutDirectories(root, environment.platform);
    const directory =
      layouts.find((candidate) => inspectDirectory(candidate, environment.platform).files) ??
      layouts.find((candidate) => existsSync(candidate)) ??
      root;
    const selected = attempt('explicit', directory);

    if (selected) return selected;

    throw new FfmpegLoadError(considered, true, environment.platform);
  }

  const searched = environment.systemDirectories
    ? [...environment.systemDirectories]
    : systemDirectories(environment);
  // A directory is worth reporting if it holds any FFmpeg library, including other ABI majors, so
  // that an FFmpeg 7 installation is explained rather than looking like "nothing installed".
  const withLibraries = searched.filter((directory) => {
    const inspection = inspectDirectory(directory, environment.platform);

    return inspection.found > 0 || inspection.otherMajors.length > 0;
  });

  for (const directory of withLibraries) {
    const selected = attempt('system', directory);

    if (selected) return selected;
  }

  if (withLibraries.length === 0) {
    considered.push({
      source: 'system',
      directory: null,
      outcome: 'rejected',
      reason: 'not-found',
      detail: `No FFmpeg 9 libraries in ${searched.length} system library directories (${searched.join(', ')}).`,
    });
  }

  const managed = managedPrefix(environment);

  if (!existsSync(managed)) {
    considered.push({
      source: 'managed',
      directory: managed,
      outcome: 'rejected',
      reason: 'not-found',
      detail: 'The Remedia installer has not installed FFmpeg here.',
    });
  } else {
    const layouts = layoutDirectories(managed, environment.platform);
    const directory =
      layouts.find((candidate) => inspectDirectory(candidate, environment.platform).files) ??
      layouts[0]!;
    const selected = attempt('managed', directory);

    if (selected) return selected;
  }

  throw new FfmpegLoadError(considered, false, environment.platform);
}

function describeFailure(
  candidates: readonly FfmpegCandidateReport[],
  explicitOverride: boolean,
  platform: NodeJS.Platform,
): string {
  const required = FFMPEG_LIBRARIES.map(({ name, major }) => `lib${name} ${major}`).join(', ');
  const lines = [
    explicitOverride
      ? 'REMEDIA_FFMPEG_DIR does not point to a loadable FFmpeg 9 installation, and no other installation was tried.'
      : 'Remedia could not load a compatible FFmpeg 9 runtime.',
    `Required ABI majors: ${required}.`,
    'Considered:',
  ];

  for (const candidate of candidates) {
    const label = candidate.source === 'explicit' ? 'REMEDIA_FFMPEG_DIR' : candidate.source;
    const location = candidate.directory ?? '(see detail)';
    const status =
      candidate.outcome === 'selected'
        ? 'selected'
        : `rejected${candidate.reason ? ` (${candidate.reason})` : ''}`;

    lines.push(
      `  - ${label}: ${location} -> ${status}${candidate.detail ? `: ${candidate.detail}` : ''}`,
    );
  }

  lines.push(
    '',
    'Install FFmpeg 9 with the Remedia installer:',
    `  ${platform === 'win32' ? WINDOWS_INSTALL_COMMAND : UNIX_INSTALL_COMMAND}`,
    platform === 'win32'
      ? 'To use another FFmpeg 9 prefix, set REMEDIA_FFMPEG_DIR to the directory that contains Library\\bin.'
      : 'To use another FFmpeg 9 prefix, set REMEDIA_FFMPEG_DIR to the directory that contains lib/.',
  );

  return lines.join('\n');
}
