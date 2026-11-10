import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, test } from 'vitest';

import { bootstrapCandidates, platformSuffixes } from '../lib/bootstrap-locator.js';

let root: string;

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), 'remedia native binding '));
});

afterEach(() => {
  rmSync(root, { recursive: true, force: true });
});

describe('platform suffixes', () => {
  test('mirror the generated napi-rs loader for every published target', () => {
    assert.deepEqual(
      platformSuffixes('linux', 'x64', () => false),
      ['linux-x64-gnu'],
    );
    assert.deepEqual(
      platformSuffixes('linux', 'x64', () => true),
      ['linux-x64-musl'],
    );
    assert.deepEqual(
      platformSuffixes('linux', 'arm64', () => false),
      ['linux-arm64-gnu'],
    );
    assert.deepEqual(
      platformSuffixes('linux', 'arm', () => false),
      ['linux-arm-gnueabihf'],
    );
    assert.deepEqual(platformSuffixes('darwin', 'arm64'), ['darwin-universal', 'darwin-arm64']);
    assert.deepEqual(platformSuffixes('darwin', 'x64'), ['darwin-universal', 'darwin-x64']);
    assert.deepEqual(platformSuffixes('win32', 'x64'), ['win32-x64-msvc']);
    assert.deepEqual(platformSuffixes('win32', 'ia32'), ['win32-ia32-msvc']);
    assert.deepEqual(platformSuffixes('win32', 'arm64'), ['win32-arm64-msvc']);
    assert.deepEqual(platformSuffixes('freebsd', 'x64'), ['freebsd-x64']);
    assert.deepEqual(platformSuffixes('android', 'arm64'), ['android-arm64']);
    assert.deepEqual(platformSuffixes('android', 'arm'), ['android-arm-eabi']);
  });

  test('are empty for platforms without a published addon', () => {
    assert.deepEqual(platformSuffixes('aix', 'ppc64'), []);
    assert.deepEqual(
      platformSuffixes('linux', 'riscv64', () => false),
      [],
    );
  });
});

describe('bootstrap lookup', () => {
  test('prefers the file next to native.js, then the platform package', () => {
    const packageRoot = join(root, 'node package');
    const platformPackage = join(root, 'platform package');

    mkdirSync(packageRoot, { recursive: true });
    mkdirSync(platformPackage, { recursive: true });
    writeFileSync(join(packageRoot, 'bootstrap.linux-x64-gnu.node'), '');
    writeFileSync(join(platformPackage, 'bootstrap.linux-x64-gnu.node'), '');

    const candidates = bootstrapCandidates(packageRoot, ['linux-x64-gnu'], (specifier) => {
      assert.equal(specifier, '@remedia/node-linux-x64-gnu/package.json');

      return join(platformPackage, 'package.json');
    });

    assert.deepEqual(candidates, [
      join(packageRoot, 'bootstrap.linux-x64-gnu.node'),
      join(platformPackage, 'bootstrap.linux-x64-gnu.node'),
    ]);
  });

  test('returns nothing when neither location exists, as on a platform without a bootstrap', () => {
    const candidates = bootstrapCandidates(root, ['linux-x64-gnu'], () => {
      throw new Error('Cannot find module');
    });

    assert.deepEqual(candidates, []);
  });

  test('ignores a platform package that has no bootstrap file', () => {
    const platformPackage = join(root, 'old platform package');

    mkdirSync(platformPackage, { recursive: true });

    assert.deepEqual(
      bootstrapCandidates(root, ['darwin-arm64'], () => join(platformPackage, 'package.json')),
      [],
    );
  });
});
