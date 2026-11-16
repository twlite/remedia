import { createRequire } from 'node:module';
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
const require = createRequire(import.meta.url);
const binding = require(resolve(process.argv[2]));
const info = JSON.parse(binding.bundledFfmpegInfo());
assert.match(info.license, /GPL version 3/);
assert.ok(info.capabilities.decoders.includes('pcm_s16le'));
assert.ok(info.capabilities.protocols.includes('https'));
const engine = await binding.NativeRemedia.create();
const probe = engine.createProbe();
const media = await engine.probe(
  probe,
  { kind: 'file', value: resolve('crates/remedia-core/tests/fixtures/generated-tone.mp3') },
  5000,
);
assert.equal(media.codec, 'mp3');
engine.cancelProbe(probe);
await engine.close();
console.log(JSON.stringify(info));
