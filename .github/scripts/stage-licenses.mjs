import { copyFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
for (const name of [
  'LICENSE',
  'THIRD_PARTY_NOTICES.md',
  'THIRD_PARTY_LICENSES.txt',
  ...(process.cwd().endsWith('remedia-node')
    ? ['LICENSES.md', 'COPYING.GPLv3', 'SOURCE_DISTRIBUTION.md']
    : []),
]) {
  copyFileSync(resolve(root, name), resolve(process.cwd(), name));
}
