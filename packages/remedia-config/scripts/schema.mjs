import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { format } from 'oxfmt';
import { generateJsonSchema } from '../lib/index.ts';

const paths = [
  new URL('../remedia.config.schema.json', import.meta.url),
  new URL('../../../apps/docs/public/schemas/remedia.config.schema.json', import.meta.url),
];
const formatted = await format('remedia.config.schema.json', JSON.stringify(generateJsonSchema()), {
  printWidth: 100,
  tabWidth: 2,
});
if (formatted.errors.length) throw new Error('Schema formatting failed');
const output = formatted.code;
for (const path of paths) {
  if (process.argv.includes('--check')) {
    if ((await readFile(path, 'utf8')) !== output)
      throw new Error(`Generated schema is stale: ${path.pathname}`);
  } else {
    await mkdir(new URL('.', path), { recursive: true });
    await writeFile(path, output);
  }
}
