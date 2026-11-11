import assert from 'node:assert/strict';
import test from 'node:test';
import { compareVersions, planTags, publishedVersionsFrom } from './docker-tags.mjs';

test('orders versions by SemVer precedence', () => {
  const ordered = ['0.1.0-dev.2', '0.1.0-dev.10', '0.1.0-rc.1', '0.1.0', '0.1.1-dev.0', '0.2.0'];

  for (let index = 1; index < ordered.length; index += 1) {
    assert.ok(compareVersions(ordered[index], ordered[index - 1]) > 0, ordered[index]);
    assert.ok(compareVersions(ordered[index - 1], ordered[index]) < 0, ordered[index]);
  }

  assert.equal(compareVersions('0.1.0', '0.1.0'), 0);
});

test('a newest prerelease owns dev and never latest', () => {
  const plan = planTags('0.1.0-dev.6', ['0.1.0-dev.5', '0.1.0-dev.6']);

  assert.deepEqual(plan, { version: '0.1.0-dev.6', prerelease: true, movingTag: 'dev' });
});

test('a newest stable release owns latest', () => {
  const plan = planTags('0.1.0', ['0.1.0-dev.6', '0.1.0']);

  assert.deepEqual(plan, { version: '0.1.0', prerelease: false, movingTag: 'latest' });
});

test('a prerelease newer than the stable release does not touch latest', () => {
  assert.equal(planTags('0.2.0-dev.0', ['0.1.0', '0.2.0-dev.0']).movingTag, 'dev');
  assert.equal(planTags('0.1.0', ['0.1.0', '0.2.0-dev.0']).movingTag, 'latest');
});

test('older releases cannot move a channel tag backwards', () => {
  assert.equal(planTags('0.1.0-dev.5', ['0.1.0-dev.5', '0.1.0-dev.6']).movingTag, null);
  assert.equal(planTags('0.1.0', ['0.1.0', '0.1.1']).movingTag, null);
});

test('rejects unpublished or malformed versions', () => {
  assert.throws(() => planTags('0.1.0-dev.7', ['0.1.0-dev.6']), /not published/);
  assert.throws(() => planTags('latest', ['latest']), /Invalid release version/);
});

test('ignores drafts and foreign tags when collecting published versions', () => {
  const versions = publishedVersionsFrom([
    { tag_name: 'v0.1.0-dev.6', draft: false },
    { tag_name: 'v0.1.0-dev.7', draft: true },
    { tag_name: 'nightly', draft: false },
    { tag_name: 'v0.1.0', draft: false },
  ]);

  assert.deepEqual(versions, ['0.1.0-dev.6', '0.1.0']);
});
