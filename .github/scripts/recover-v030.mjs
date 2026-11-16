import { writeFileSync } from 'node:fs';
import { RELEASE_TARGETS } from './release.mjs';

const repository = process.env.GITHUB_REPOSITORY;
const token = process.env.GH_TOKEN;
if (!repository || !token) throw new Error('Missing GitHub credentials');
async function get(path) {
  for (let attempt = 0; attempt < 4; attempt++) {
    try {
      const response = await fetch('https://api.github.com/repos/' + repository + path, {
        headers: { Authorization: 'Bearer ' + token, Accept: 'application/vnd.github+json' },
        signal: AbortSignal.timeout(20000),
      });
      if (!response.ok) throw new Error('GitHub GET ' + path + ': HTTP ' + response.status);
      return await response.json();
    } catch (error) {
      if (attempt === 3) throw error;
      await new Promise(resolve => setTimeout(resolve, 1000 * 2 ** attempt));
    }
  }
}
const run = await get('/actions/runs/43813499144');
const tag = await get('/git/ref/tags/v0.3.0');
if (run.head_sha !== 'a3a4e876636345ebc2245c2bc26befdc279944f7' ||
    tag.object.sha !== run.head_sha) throw new Error('Source run and immutable release tag differ');
const releases = await get('/releases?per_page=100');
const release = releases.find(item => item.tag_name === 'v0.3.0');
if (!release || !release.draft) throw new Error('v0.3.0 draft is missing or no longer a draft');
const assets = await get('/releases/' + release.id + '/assets?per_page=100');
const names = RELEASE_TARGETS.flatMap(target => [
  'node.' + target.suffix + '.node',
  ...(target.server === false ? [] : [
    'remedia-server-' + target.triple + (target.extension ?? ''),
    'remedia-server-' + target.triple + '.tar.gz',
  ]),
]).concat('LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt',
          'LICENSES.md', 'COPYING.GPLv3', 'corresponding-source.tar.gz');
if (names.length !== 35 || assets.length !== names.length || new Set(names).size !== names.length)
  throw new Error('Unexpected release asset count or duplicate names');
const manifest = names.map(name => {
  const asset = assets.find(item => item.name === name);
  if (!asset || asset.state !== 'uploaded' || !Number.isSafeInteger(asset.size) ||
      asset.size < 1 || !/^sha256:[a-f0-9]{64}$/.test(asset.digest ?? ''))
    throw new Error('Missing, incomplete, or unhashed release asset: ' + name);
  return { name, size: asset.size, sha256: asset.digest.slice(7) };
});
console.log('Verified ' + manifest.length + ' existing release assets');
writeFileSync(process.env.RUNNER_TEMP + '/github-release-assets.json',
              JSON.stringify(manifest, null, 2) + '\n');
