import { appendFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseReleaseVersion } from './release.mjs';

/**
 * Compare two release versions by SemVer precedence.
 * @param {string} left A version accepted by parseReleaseVersion.
 * @param {string} right A version accepted by parseReleaseVersion.
 * @returns {number} Negative when left is older, positive when newer, zero when equal.
 */
export function compareVersions(left, right) {
  const split = (version) => {
    const [core, pre] = version.split(/-(.*)/s);

    return { core: core.split('.').map(Number), pre: pre === undefined ? null : pre.split('.') };
  };
  const a = split(left);
  const b = split(right);

  for (let index = 0; index < 3; index += 1) {
    if (a.core[index] !== b.core[index]) return a.core[index] - b.core[index];
  }

  // A version without a prerelease part outranks every prerelease of the same core.
  if (a.pre === null || b.pre === null) return (a.pre === null) - (b.pre === null);

  for (let index = 0; index < Math.max(a.pre.length, b.pre.length); index += 1) {
    const x = a.pre[index];
    const y = b.pre[index];

    if (x === undefined) return -1;

    if (y === undefined) return 1;

    if (x === y) continue;

    const numeric = [/^\d+$/.test(x), /^\d+$/.test(y)];

    if (numeric[0] && numeric[1]) return Number(x) - Number(y);

    if (numeric[0]) return -1;

    if (numeric[1]) return 1;

    return x < y ? -1 : 1;
  }

  return 0;
}

/**
 * Decide which image tags a published release owns.
 *
 * The exact version tag is always owned by the release. The moving tag belongs to a
 * release channel: prereleases move `dev`, stable releases move `latest`. A release only
 * moves its channel's tag when no newer published release exists in that channel, so
 * retrying an old release never rewinds it.
 * @param {string} version Release version without the leading "v".
 * @param {string[]} publishedVersions Versions of all public releases.
 * @returns {{ version: string, prerelease: boolean, movingTag: string | null }}
 */
export function planTags(version, publishedVersions) {
  const { prerelease } = parseReleaseVersion(version);

  if (!publishedVersions.includes(version)) {
    throw new Error(`Release v${version} is not published, so its image cannot be built from it.`);
  }

  const newerInChannel = publishedVersions.some(
    (other) =>
      parseReleaseVersion(other).prerelease === prerelease && compareVersions(other, version) > 0,
  );

  return { version, prerelease, movingTag: newerInChannel ? null : prerelease ? 'dev' : 'latest' };
}

/** Collect the versions of public (non-draft) releases whose tags are valid release versions. */
export function publishedVersionsFrom(releases) {
  const versions = [];

  for (const release of releases) {
    if (release.draft || typeof release.tag_name !== 'string') continue;

    const version = release.tag_name.replace(/^v/, '');

    try {
      parseReleaseVersion(version);
      versions.push(version);
    } catch {
      // Ignore tags that are not Remedia release versions.
    }
  }

  return versions;
}

async function fetchReleases(repository, token, fetchImpl = fetch) {
  const releases = [];

  for (let page = 1; page <= 10; page += 1) {
    const response = await fetchImpl(
      `https://api.github.com/repos/${repository}/releases?per_page=100&page=${page}`,
      {
        headers: {
          accept: 'application/vnd.github+json',
          ...(token ? { authorization: `Bearer ${token}` } : {}),
        },
      },
    );

    if (!response.ok) throw new Error(`GitHub release lookup failed (HTTP ${response.status})`);

    const batch = await response.json();

    releases.push(...batch);

    if (batch.length < 100) break;
  }

  return releases;
}

async function main() {
  const [command] = process.argv.slice(2);

  if (command !== 'plan') throw new Error('Usage: docker-tags.mjs plan');

  const version = (process.env.RELEASE_VERSION ?? '').replace(/^v/, '');
  const releases = await fetchReleases(process.env.GITHUB_REPOSITORY, process.env.GH_TOKEN);
  const plan = planTags(version, publishedVersionsFrom(releases));
  const output = [
    `version=${plan.version}`,
    `prerelease=${plan.prerelease}`,
    `moving_tag=${plan.movingTag ?? ''}`,
  ].join('\n');

  if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, output + '\n');
  else process.stdout.write(output + '\n');
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.message : error);
    process.exit(1);
  });
}
