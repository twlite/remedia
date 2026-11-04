import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { changedPaths, needsNativeChecks } from './ci-changes.mjs';

test('app changes, shared npm lock updates, and documentation skip native checks', () => {
  for (const path of [
    'apps/docs/content/docs/node/distribution.mdx',
    'apps/docs/package.json',
    'apps/docs/src/app/page.tsx',
    'apps/discord/src/index.ts',
    'apps/discord/package.json',
    'pnpm-lock.yaml',
    'README.md',
    'crates/remedia-core/README.md',
    'packages/remedia-node/README.md',
    'THIRD_PARTY_NOTICES.md',
    'LICENSE',
  ])
    assert.equal(needsNativeChecks([path]), false, path);
});

test('engine, native integration, toolchain, and build configuration changes run native checks', () => {
  for (const path of [
    'crates/remedia-core/src/lib.rs',
    'crates/remedia-server/Cargo.toml',
    'packages/remedia-node/src/lib.rs',
    'packages/remedia-node/lib/index.ts',
    'packages/remedia-client/tests/server-integration.test.ts',
    'Cargo.lock',
    'Cargo.toml',
    'rust-toolchain.toml',
    '.cargo/config.toml',
    '.github/native-deps/ffmpeg7.yml',
    '.github/workflows/platform.yaml',
    '.github/scripts/release.mjs',
    'package.json',
    'pnpm-workspace.yaml',
    'tsconfig.base.json',
  ])
    assert.equal(needsNativeChecks(['apps/docs/package.json', path]), true, path);
});

test('manual runs and unavailable push revisions request full validation', () => {
  assert.equal(changedPaths('workflow_dispatch', {}), null);
  assert.equal(changedPaths('push', {}), null);
  assert.equal(
    changedPaths('push', { before: '0'.repeat(40), after: 'head', ref: 'refs/heads/main' }),
    null,
  );
  assert.equal(
    changedPaths('push', { before: 'base', after: 'head' }, () => {
      throw new Error('unavailable');
    }),
    null,
  );
});

test('push comparison covers the complete push and preserves unusual filenames', () => {
  const paths = changedPaths('push', { before: 'before', after: 'after' }, (args) => {
    assert.deepEqual(args, ['diff', '--name-only', '--no-renames', '-z', 'before', 'after', '--']);
    return 'apps/docs/a b.mdx\0crates/remedia-core/new\nfile.rs\0';
  });
  assert.equal(needsNativeChecks(paths), true);
  assert.equal(paths.length, 2);
});

test('pull requests compare against their merge base', () => {
  assert.deepEqual(
    changedPaths(
      'pull_request',
      { pull_request: { base: { sha: 'base' }, head: { sha: 'head' } } },
      (args) => {
        assert.deepEqual(args, ['diff', '--name-only', '--no-renames', '-z', 'base...head', '--']);
        return 'apps/discord/src/index.ts\0';
      },
    ),
    ['apps/discord/src/index.ts'],
  );
});

test('new feature branches compare with the default branch instead of rebuilding Rust for apps', () => {
  const calls = [];
  const paths = changedPaths(
    'push',
    {
      before: '0'.repeat(40),
      after: 'head',
      ref: 'refs/heads/fix/docs',
      repository: { default_branch: 'main' },
    },
    (args) => {
      calls.push(args);
      return args[0] === 'merge-base' ? 'base\n' : 'apps/docs/package.json\0';
    },
  );
  assert.deepEqual(calls[0], ['merge-base', 'refs/remotes/origin/main', 'head']);
  assert.equal(needsNativeChecks(paths), false);
});

test('real git history catches native deletions and renames into ignored documentation', () => {
  const directory = mkdtempSync(join(tmpdir(), 'remedia-ci-paths-'));
  const git = (args) =>
    execFileSync('git', args, {
      cwd: directory,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
    });
  try {
    git(['init', '-q']);
    git(['config', 'user.name', 'CI fixture']);
    git(['config', 'user.email', 'ci@example.invalid']);
    mkdirSync(join(directory, 'crates/core/src'), { recursive: true });
    writeFileSync(join(directory, 'crates/core/src/lib.rs'), 'fixture');
    git(['add', '.']);
    git(['commit', '-qm', 'initial']);
    const before = git(['rev-parse', 'HEAD']).trim();
    git(['mv', 'crates/core/src/lib.rs', 'README.md']);
    git(['commit', '-qm', 'move']);
    const after = git(['rev-parse', 'HEAD']).trim();
    const paths = changedPaths('push', { before, after }, git);
    assert.ok(paths.includes('crates/core/src/lib.rs'));
    assert.equal(needsNativeChecks(paths), true);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
