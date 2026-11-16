// Regenerate NAPI-RS's embedded, unsupported-target helper from source instead of shipping an object.
import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join, resolve } from 'node:path';

const vendor = resolve(process.argv[2]);
for (const name of readdirSync(vendor).filter((entry) => /^napi-build(?:-|$)/.test(entry))) {
  const root = join(vendor, name);
  const source = join(root, 'src/wasi_heap_sync_exports.c');
  if (!existsSync(source)) throw new Error('Missing NAPI-RS helper C source');
  const object = join(root, 'src/wasi_heap_sync_exports.o');
  execFileSync(
    process.env.REMEDIA_WASI_CC || 'clang',
    ['--target=wasm32-wasip1-threads', '-O2', '-c', source, '-o', object],
    { stdio: 'inherit' },
  );
  const checksumPath = join(root, '.cargo-checksum.json');
  const checksum = JSON.parse(readFileSync(checksumPath, 'utf8'));
  checksum.files['src/wasi_heap_sync_exports.o'] = createHash('sha256')
    .update(readFileSync(object))
    .digest('hex');
  writeFileSync(checksumPath, JSON.stringify(checksum));
}
