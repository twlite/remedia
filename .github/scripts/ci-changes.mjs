import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

export function needsChecks(paths) {
  return paths.some((path) => {
    if (path.startsWith('apps/docs/content/')) return true;
    if (['LICENSE', 'THIRD_PARTY_NOTICES.md', 'THIRD_PARTY_LICENSES.txt'].includes(path))
      return true;
    if (/\.md$/i.test(path)) return false;
    return (
      [
        'crates/',
        'packages/',
        'apps/docs/',
        'apps/discord/',
        'scripts/docs/',
        '.cargo/',
        '.github/workflows/',
        '.github/scripts/',
        '.github/native-deps/',
      ].some((prefix) => path.startsWith(prefix)) ||
      [
        'Cargo.toml',
        'Cargo.lock',
        'rust-toolchain.toml',
        'package.json',
        'pnpm-lock.yaml',
        'pnpm-workspace.yaml',
        'tsconfig.base.json',
        '.oxlintrc.json',
        '.oxfmtrc.json',
        '.editorconfig',
        '.gitattributes',
        '.gitignore',
      ].includes(path)
    );
  });
}

export function needsNativeChecks(paths) {
  return paths.some((path) => {
    if (/\.md$/i.test(path)) return false;
    return (
      path.startsWith('crates/') ||
      path.startsWith('packages/remedia-node/') ||
      path.startsWith('packages/remedia-client/') ||
      path.startsWith('packages/remedia-config/') ||
      path.startsWith('.cargo/') ||
      path.startsWith('.github/native-deps/') ||
      path.startsWith('.github/scripts/') ||
      path.startsWith('.github/workflows/') ||
      [
        'Cargo.toml',
        'Cargo.lock',
        'rust-toolchain.toml',
        'package.json',
        'pnpm-workspace.yaml',
        'tsconfig.base.json',
      ].includes(path)
    );
  });
}

export function changedPaths(
  eventName,
  event,
  git = (args) => execFileSync('git', args, { encoding: 'utf8' }),
) {
  if (eventName === 'workflow_dispatch') return null;
  let base = eventName === 'pull_request' ? event.pull_request.base.sha : event.before;
  const head = eventName === 'pull_request' ? event.pull_request.head.sha : event.after;
  if (!base || !head) return null;
  try {
    if (/^0+$/.test(base)) {
      const defaultBranch = event.repository?.default_branch ?? 'main';
      // A new feature branch can still contain only app or documentation changes.
      if (event.ref === 'refs/heads/' + defaultBranch) return null;
      base = git(['merge-base', 'refs/remotes/origin/' + defaultBranch, head]).trim();
      if (!base) return null;
    }
    return git([
      'diff',
      '--name-only',
      '--no-renames',
      '-z',
      eventName === 'pull_request' ? base + '...' + head : base,
      ...(eventName === 'pull_request' ? [] : [head]),
      '--',
    ])
      .split('\0')
      .filter(Boolean);
  } catch {
    console.warn('Unable to compare event revisions; running all native checks.');
    return null;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const event = JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8'));
  const paths = changedPaths(process.env.GITHUB_EVENT_NAME, event);
  const native = paths === null || needsNativeChecks(paths);
  const checks = paths === null || needsChecks(paths);
  const output = 'native=' + native + '\nchecks=' + checks + '\n';
  if (process.env.GITHUB_OUTPUT) writeFileSync(process.env.GITHUB_OUTPUT, output, { flag: 'a' });
  else process.stdout.write(output);
}
