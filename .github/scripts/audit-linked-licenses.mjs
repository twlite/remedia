import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';

// Explicitly reviewed expressions from Cargo.lock, rather than accepting any unknown OR branch.
// This is a metadata guard: it does not replace source/license review of bundled C code.
const allowed = new Set([
  '(MIT OR Apache-2.0) AND Unicode-3.0',
  'Apache-2.0',
  'Apache-2.0 / MIT',
  'Apache-2.0 AND ISC',
  'Apache-2.0 OR ISC OR MIT',
  'Apache-2.0 OR MIT',
  'Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT',
  'Apache-2.0/MIT',
  'BSD-2-Clause OR Apache-2.0 OR MIT',
  'BSD-3-Clause',
  'CDLA-Permissive-2.0',
  'ISC',
  'MIT',
  'MIT OR Apache-2.0',
  'MIT OR Apache-2.0 OR BSD-1-Clause',
  'MIT OR Apache-2.0 OR LGPL-2.1-or-later',
  'MIT/Apache-2.0',
  'MPL-2.0',
  'Unicode-3.0',
  'Unlicense OR MIT',
  'Unlicense/MIT',
  'WTFPL',
  'Zlib',
]);
const metadata = JSON.parse(
  execFileSync('cargo', ['metadata', '--locked', '--format-version=1'], {
    encoding: 'utf8',
    maxBuffer: 32 * 1024 * 1024,
  }),
);
for (const dependency of metadata.packages) {
  if (!allowed.has(dependency.license))
    throw new Error(
      `Unaudited license ${dependency.license}: ${dependency.name} ${dependency.version}`,
    );
}

const output = process.argv[2] ?? 'target/linked-license-inventory.json';
mkdirSync(dirname(output), { recursive: true });
writeFileSync(
  output,
  JSON.stringify(
    metadata.packages.map(({ name, version, source, license }) => ({
      name,
      version,
      source,
      license,
    })),
    null,
    2,
  ) + '\n',
);
