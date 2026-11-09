import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { expect, it } from 'vitest';
const require = createRequire(import.meta.url);
it('packs generated declarations and imports from a separate TypeScript consumer', () => {
  const dir = mkdtempSync(join(tmpdir(), 'remedia-host-consumer-'));
  try {
    execFileSync('pnpm', ['pack', '--out', join(dir, 'host.tgz')], {
      cwd: resolve('.'),
      stdio: 'pipe',
    });
    const pkg = join(dir, 'node_modules/@remedia/plugin-host');
    mkdirSync(pkg, { recursive: true });
    execFileSync('tar', ['-xzf', join(dir, 'host.tgz'), '--strip-components=1', '-C', pkg]);
    mkdirSync(join(dir, 'node_modules/@types'), { recursive: true });
    symlinkSync(
      dirname(require.resolve('@types/node/package.json')),
      join(dir, 'node_modules/@types/node'),
    );
    writeFileSync(join(dir, 'package.json'), JSON.stringify({ type: 'module' }));
    writeFileSync(
      join(dir, 'consumer.ts'),
      `
import { createPluginHost, type Operation, type ExecuteResult } from '@remedia/plugin-host';
import { createNodeMiddleware } from '@remedia/plugin-host/node';
declare module '@remedia/plugin-host' { interface PluginOperations { globallyTyped: Operation<string, number> } }
const augmented = createPluginHost({ name: 'Augmented' });
const globalResult: Promise<ExecuteResult<number>> = augmented.handle({pluginId:'global', operation:'globallyTyped', input:'ok'});
// @ts-expect-error Augmented input is string
augmented.handle({pluginId:'global', operation:'globallyTyped', input: 1});
void globalResult;
type Schema = { resolve: Operation<{ id: string }, { title: string }> };
const host = createPluginHost<Schema>({ name: 'Consumer' });
host.register<Schema>({ id: 'example', operations: { resolve: input => ({ title: input.id }) } });
const result: Promise<ExecuteResult<{ title: string }>> = host.handle({ pluginId: 'example', operation: 'resolve', input: { id: 'x' } });
// @ts-expect-error Input must match schema
host.handle({ pluginId: 'example', operation: 'resolve', input: 123 });
createNodeMiddleware(host);
void result;
`,
    );
    for (const moduleKind of ['NodeNext', 'ESNext']) {
      writeFileSync(
        join(dir, 'tsconfig.json'),
        JSON.stringify({
          compilerOptions: {
            target: 'ES2022',
            module: moduleKind,
            moduleResolution: moduleKind === 'NodeNext' ? 'NodeNext' : 'Bundler',
            strict: true,
            exactOptionalPropertyTypes: true,
            noEmit: true,
            skipLibCheck: false,
            types: ['node'],
          },
          include: ['consumer.ts'],
        }),
      );
      execFileSync(
        process.execPath,
        [require.resolve('typescript/bin/tsc'), '-p', join(dir, 'tsconfig.json')],
        { stdio: 'pipe' },
      );
    }
    writeFileSync(
      join(dir, 'runtime.mjs'),
      `import { createPluginHost } from '@remedia/plugin-host'; import { createNodeMiddleware } from '@remedia/plugin-host/node'; const h = createPluginHost({name:'x'}); if(typeof createNodeMiddleware(h)!=='function') throw Error('adapter'); await h.close();`,
    );
    execFileSync(process.execPath, [join(dir, 'runtime.mjs')], { stdio: 'pipe' });
    expect(readFileSync(join(pkg, 'dist/index.d.ts'), 'utf8')).toContain('class PluginHost');
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}, 30000);
