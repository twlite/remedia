import { describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { generateJsonSchema, RemediaConfigSchema } from '../lib/index.js';

const cases = JSON.parse(readFileSync(new URL('./parity.json', import.meta.url), 'utf8')) as {
  name: string;
  config: unknown;
  valid: boolean;
}[];
describe('shared Rust/Zod configuration corpus', () => {
  for (const sample of cases) {
    it(sample.name, () =>
      expect(RemediaConfigSchema.safeParse(sample.config).success).toBe(sample.valid),
    );
  }
});

it('applies native defaults without expanding environment references', () => {
  const result = RemediaConfigSchema.parse({
    users: [{ username: 'bot', password: "env('SECRET')" }],
  });
  expect(result.listen).toBe('127.0.0.1:8787');
  expect(result.audio.opus).toEqual({ bitrate: 128000, complexity: 9, vbr: true });
  expect(result.audio.crossfade).toEqual({
    enabled: false,
    durationMs: 5000,
    prepareBeforeMs: 30000,
    curve: 'equalPower',
  });
  expect(result.users![0]?.password).toBe("env('SECRET')");
});

it('generates strict objects and editor metadata with current defaults', () => {
  const schema = generateJsonSchema();
  expect(schema.$schema).toBe('https://json-schema.org/draft/2020-12/schema');
  expect(schema.additionalProperties).toBe(false);
  expect(schema.oneOf).toEqual([{ required: ['auth'] }, { required: ['users'] }]);
  expect(JSON.stringify(schema)).toContain('128000');
});

it('matches the shared Rust default expansion fixture', () => {
  const expected = JSON.parse(
    readFileSync(new URL('./defaults.json', import.meta.url), 'utf8'),
  ) as unknown;
  expect(RemediaConfigSchema.parse({ users: [{ username: 'bot', password: 'example' }] })).toEqual(
    expected,
  );
});

it('ships identical package and documentation schemas and links the example to their ID', () => {
  const artifact = readFileSync(new URL('../remedia.config.schema.json', import.meta.url), 'utf8');
  const docs = readFileSync(
    new URL('../../../apps/docs/public/schemas/remedia.config.schema.json', import.meta.url),
    'utf8',
  );
  const example = JSON.parse(
    readFileSync(
      new URL('../../../crates/remedia-server/remedia.config.example.json', import.meta.url),
      'utf8',
    ),
  ) as { $schema: string };
  expect(docs).toBe(artifact);
  expect(example.$schema).toBe(generateJsonSchema().$id);
});
