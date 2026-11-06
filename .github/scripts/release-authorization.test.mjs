import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { authorizeRelease } from './release-authorization.mjs';
import { detectRelease } from './release.mjs';

const environment = {
  GITHUB_EVENT_NAME: 'push',
  GITHUB_REF: 'refs/heads/main',
  GITHUB_REPOSITORY: 'twlite/remedia',
  GITHUB_ACTOR: 'collaborator',
  GITHUB_TRIGGERING_ACTOR: 'collaborator',
  GH_TOKEN: 'fixture-token',
};

test('push and dispatch release candidates require main and an exact release subject', () => {
  for (const eventName of ['push', 'workflow_dispatch']) {
    assert.equal(
      detectRelease({ eventName, ref: 'refs/heads/main', subject: 'chore: release 1.2.3' }).version,
      '1.2.3',
    );
    assert.equal(
      detectRelease({ eventName, ref: 'refs/heads/feature', subject: 'chore: release 1.2.3' }),
      null,
    );
    assert.equal(
      detectRelease({ eventName, ref: 'refs/heads/main', subject: 'feat: ordinary change' }),
      null,
    );
  }
});

test('allows current write, maintain and admin collaborators for both trigger paths', async () => {
  for (const eventName of ['push', 'workflow_dispatch']) {
    for (const permission of ['write', 'maintain', 'admin']) {
      const calls = [];
      await authorizeRelease({
        env: { ...environment, GITHUB_EVENT_NAME: eventName },
        request: async (url, options) => {
          calls.push(url);
          assert.equal(options.headers.Authorization, 'Bearer fixture-token');
          assert.ok(options.signal instanceof AbortSignal);
          assert.equal(options.redirect, 'error');
          return { status: 200, json: async () => ({ permission }) };
        },
      });
      assert.deepEqual(calls, [
        'https://api.github.com/repos/twlite/remedia/collaborators/collaborator/permission',
      ]);
    }
  }
});

test('denies read-only, triage, unknown and malformed permissions', async () => {
  for (const permission of ['read', 'triage', 'none', '', undefined]) {
    await assert.rejects(
      authorizeRelease({
        env: environment,
        request: async () => ({ status: 200, json: async () => ({ permission }) }),
      }),
      /must have repository/,
    );
  }
});

test('checks both original actor and rerun actor, denying either unauthorized identity', async () => {
  for (const denied of ['collaborator', 'rerunner']) {
    const calls = [];
    await assert.rejects(
      authorizeRelease({
        env: { ...environment, GITHUB_TRIGGERING_ACTOR: 'rerunner' },
        request: async (url) => {
          calls.push(url);
          return {
            status: 200,
            json: async () => ({
              permission: url.includes(`/collaborators/${denied}/`) ? 'read' : 'write',
            }),
          };
        },
      }),
      /must have repository/,
    );
    assert.equal(calls.length, denied === 'collaborator' ? 1 : 2);
  }
});

test('fails closed on missing context, non-main refs and unsupported events without API calls', async () => {
  const variants = Object.keys(environment).map((key) => ({ ...environment, [key]: '' }));
  variants.push(
    { ...environment, GITHUB_REF: 'refs/heads/fix/release' },
    { ...environment, GITHUB_EVENT_NAME: 'pull_request' },
  );
  for (const env of variants) {
    await assert.rejects(
      authorizeRelease({ env, request: () => assert.fail('must not request GitHub') }),
    );
  }
});

test('fails closed on API denials, rate limits, outages, invalid responses and timeouts', async () => {
  for (const status of [401, 403, 404, 429, 500]) {
    await assert.rejects(
      authorizeRelease({ env: environment, request: async () => ({ status }) }),
      new RegExp(`HTTP ${status}`),
    );
  }
  for (const request of [
    async () => {
      throw new Error('request timed out');
    },
    async () => ({
      status: 200,
      json: async () => {
        throw new Error('invalid JSON');
      },
    }),
  ]) {
    await assert.rejects(authorizeRelease({ env: environment, request }));
  }
});

test('every release mutation job reauthorizes before its first release side effect', () => {
  const workflow = readFileSync(new URL('../workflows/ci.yaml', import.meta.url), 'utf8');
  for (const [job, sideEffect] of [
    ['draft-release', 'Create or verify the immutable release tag'],
    ['upload-release', 'Upload all native binaries'],
    ['publish', 'Publish in dependency order'],
    ['github-release', 'Verify every uploaded asset'],
  ]) {
    const block = workflow.split(`\n  ${job}:`)[1].split(/\n  [a-z][\w-]*:/)[0];
    assert.match(block, /GH_TOKEN: \$\{\{ github.token \}\}/);
    assert.ok(block.indexOf('release.mjs authorize') < block.indexOf(sideEffect));
    assert.ok(block.indexOf('release.mjs authorize') >= 0);
  }
  const script = readFileSync(new URL('./release.mjs', import.meta.url), 'utf8');
  assert.match(
    script,
    /if \(!release\) return writeGitHubOutput\(\{ is_release: 'false' \}\);\s+await authorizeRelease\(\);\s+return writeGitHubOutput\(\{\s+is_release: 'true'/,
  );
});
