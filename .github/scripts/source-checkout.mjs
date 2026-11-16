// Offline extraction from the retained source snapshot; no upstream fallback in offline mode.
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileHash } from './release-network.mjs';

const [name, destination] = process.argv.slice(2);
const pins = JSON.parse(
  readFileSync(new URL('../native-deps/source-inputs.json', import.meta.url), 'utf8'),
);
const pin = pins.find((item) => item.name === name);
if (!pin || !destination || !process.env.REMEDIA_SOURCE_DIR)
  throw new Error('Expected source name, destination and REMEDIA_SOURCE_DIR');
const archive = resolve(process.env.REMEDIA_SOURCE_DIR, `${name}.tar.gz`);
if ((await fileHash(archive)) !== pin.sha256) throw new Error('Source archive mismatch: ' + name);
mkdirSync(destination, { recursive: true });
execFileSync('tar', ['-xzf', archive, '--strip-components=1', '-C', destination]);
if (name === 'ffmpeg') writeFileSync(join(destination, 'VERSION'), pin.revision.slice(0, 7) + '\n');
