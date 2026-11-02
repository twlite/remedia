import assert from 'node:assert/strict';
import test from 'node:test';

import { plus100 } from '../index.js';

test('exports the native smoke-test function', () => {
  assert.equal(plus100(42), 142);
});
