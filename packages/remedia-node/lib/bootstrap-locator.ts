import { existsSync, readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';

const require = createRequire(import.meta.url);

/**
 * Whether Linux uses musl rather than glibc, mirroring the generated napi-rs loader's decision so
 * that the bootstrap comes from the same platform package as the engine addon.
 */
function isMusl(): boolean {
  // @types/node does not declare `excludeNetwork`, which keeps the report from doing DNS lookups.
  const report = process.report as
    | (NonNullable<typeof process.report> & { excludeNetwork: boolean })
    | undefined;

  if (typeof report?.getReport === 'function') {
    const previous = report.excludeNetwork;

    try {
      report.excludeNetwork = true;

      const data = report.getReport() as {
        header?: { glibcVersionRuntime?: string };
        sharedObjects?: string[];
      };

      if (data.header?.glibcVersionRuntime) return false;
      if (
        data.sharedObjects?.some(
          (object) => object.includes('libc.musl-') || object.includes('ld-musl-'),
        )
      ) {
        return true;
      }
    } catch {
      // Fall through to the filesystem check.
    } finally {
      report.excludeNetwork = previous;
    }
  }

  try {
    return readFileSync('/usr/bin/ldd', 'utf8').includes('musl');
  } catch {
    return false;
  }
}

/**
 * napi-rs platform suffixes that may hold this machine's addons, in the generated loader's order.
 *
 * @param platform - Node.js platform string.
 * @param arch - Node.js architecture string.
 * @param musl - Whether a Linux system uses musl; only consulted on Linux.
 * @returns Suffixes such as `linux-x64-gnu` or `darwin-arm64`; empty when unsupported.
 * @internal
 */
export function platformSuffixes(
  platform: NodeJS.Platform,
  arch: string,
  musl: () => boolean = isMusl,
): string[] {
  switch (platform) {
    case 'linux': {
      const libc = musl() ? 'musl' : 'gnu';

      if (arch === 'x64') return [`linux-x64-${libc}`];
      if (arch === 'arm64') return [`linux-arm64-${libc}`];
      if (arch === 'arm') return [libc === 'musl' ? 'linux-arm-musleabihf' : 'linux-arm-gnueabihf'];

      return [];
    }
    case 'darwin':
      return ['darwin-universal', `darwin-${arch}`];
    case 'win32':
      return arch === 'x64'
        ? ['win32-x64-msvc']
        : arch === 'ia32'
          ? ['win32-ia32-msvc']
          : arch === 'arm64'
            ? ['win32-arm64-msvc']
            : [];
    case 'freebsd':
      return [`freebsd-${arch}`];
    case 'android':
      return arch === 'arm64' ? ['android-arm64'] : arch === 'arm' ? ['android-arm-eabi'] : [];
    default:
      return [];
  }
}

/**
 * File locations where this platform's bootstrap addon could be: next to `native.js` in a source
 * checkout, then inside the installed `@remedia/node-<suffix>` platform package.
 *
 * @param packageRoot - Directory of the `@remedia/node` package.
 * @param suffixes - Platform suffixes from {@link platformSuffixes}.
 * @param resolvePackage - Resolves a module specifier to a file path; throws if absent.
 * @returns Existing file paths in preference order.
 * @internal
 */
export function bootstrapCandidates(
  packageRoot: string,
  suffixes: readonly string[],
  resolvePackage: (specifier: string) => string = (specifier) => require.resolve(specifier),
): string[] {
  const candidates: string[] = [];

  for (const suffix of suffixes) {
    const fileName = `bootstrap.${suffix}.node`;
    const local = join(packageRoot, fileName);

    if (existsSync(local)) candidates.push(local);

    try {
      const manifest = resolvePackage(`@remedia/node-${suffix}/package.json`);
      const packaged = join(dirname(manifest), fileName);

      if (existsSync(packaged)) candidates.push(packaged);
    } catch {
      // The optional platform package is not installed.
    }
  }

  return candidates;
}
