// The concise inventory is derived from the same pinned inputs used by builds.
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';

const pins = JSON.parse(readFileSync('.github/native-deps/source-inputs.json', 'utf8'));
const metadata = JSON.parse(
  execFileSync('cargo', ['metadata', '--locked', '--format-version=1'], {
    encoding: 'utf8',
    maxBuffer: 32 * 1024 * 1024,
  }),
);
const native = (name) => pins.find((pin) => pin.name === name).revision;
const components = [
  ['FFmpeg', native('ffmpeg'), 'All native targets; GPL-3.0-or-later with GPL/version3 flags'],
  ['OpenSSL', native('openssl'), 'Linux GNU/musl, Android, FreeBSD; Apache-2.0'],
  ['vcpkg', native('vcpkg'), 'Windows build scripts and FFmpeg port patches; MIT (build tool)'],
  ...[
    'ffmpeg-next',
    'ffmpeg-sys-next',
    'opus2',
    'libopus_sys',
    'davey',
    'openmls',
    'ring',
    'hpke-rs',
    'hpke-rs-crypto',
    'hpke-rs-libcrux',
    'hpke-rs-rust-crypto',
  ].map((name) => {
    const item = metadata.packages.find((pkg) => pkg.name === name);
    if (!item) throw new Error('Missing locked component ' + name);
    return [name, item.version, item.license];
  }),
];
const rows = [['Component', 'Pinned source/version', 'Terms and scope'], ...components].map((row) =>
  row.map((cell) => String(cell).replaceAll('|', '\\|')),
);
const widths = rows[0].map((_, index) => Math.max(...rows.map((row) => row[index].length)));
const render = (row) =>
  '| ' + row.map((cell, index) => cell.padEnd(widths[index])).join(' | ') + ' |';
const table =
  '<!-- BEGIN PINNED INVENTORY -->\n\n' +
  render(rows[0]) +
  '\n' +
  render(widths.map((width) => '-'.repeat(width))) +
  '\n' +
  rows.slice(1).map(render).join('\n') +
  '\n\n<!-- END PINNED INVENTORY -->';
const path = 'THIRD_PARTY_NOTICES.md';
const original = readFileSync(path, 'utf8');
const updated = original.replace(
  /<!-- BEGIN PINNED INVENTORY -->[^]*?<!-- END PINNED INVENTORY -->/,
  table,
);
if (!updated.includes(table)) throw new Error('Missing inventory markers');
if (process.argv.includes('--check')) {
  if (updated !== original)
    throw new Error(
      'Pinned notices are stale; run node .github/scripts/update-third-party-notices.mjs',
    );
} else writeFileSync(path, updated);
