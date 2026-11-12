import { execFileSync } from 'node:child_process';
import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';

import {
  OUTPUT_DIR,
  REPO_ROOT,
  RUSTDOC_FORMAT_VERSION,
  RUSTDOC_TOOLCHAIN,
  RUST_CRATES,
  TYPEDOC_VERSION,
  TYPESCRIPT_PACKAGES,
} from './config.ts';
import { firstParagraph, markdownToPlainText } from './docs-util.ts';
import { resolveLinks } from './links.ts';
import {
  MANIFEST_SCHEMA_VERSION,
  type ApiManifest,
  type ApiPackage,
  type ApiProject,
} from './model.ts';
import { normalizeRustdoc, readRustdocJson } from './normalize-rustdoc.ts';
import { normalizeTypeDoc, readTypeDocJson } from './normalize-typedoc.ts';
import { buildModuleEntries, buildPages } from './pages.ts';
import { runRustdoc } from './rustdoc.ts';
import { planSymbols } from './symbols.ts';
import { runTypeDoc } from './typedoc.ts';
import { validate } from './validate.ts';

/** Command-line options of `pnpm docs:api`. */
interface Options {
  only?: 'typescript' | 'rust';
  strict: boolean;
}

function parseArgs(argv: string[]): Options {
  const options: Options = { strict: false };

  for (const arg of argv) {
    if (arg === '--strict') options.strict = true;
    else if (arg === '--only=typescript') options.only = 'typescript';
    else if (arg === '--only=rust') options.only = 'rust';
    else throw new Error(`Unknown argument ${arg}. Supported: --only=typescript|rust, --strict.`);
  }

  return options;
}

/**
 * Commit the source links point at. `REMEDIA_DOCS_SOURCE_REF` overrides it; CI uses `GITHUB_SHA`.
 */
function sourceRef(): string {
  const configured = process.env.REMEDIA_DOCS_SOURCE_REF ?? process.env.GITHUB_SHA;

  if (configured) return configured;

  try {
    return execFileSync('git', ['rev-parse', 'HEAD'], { cwd: REPO_ROOT, encoding: 'utf8' }).trim();
  } catch {
    return 'main';
  }
}

async function readJson<T>(file: string): Promise<T> {
  return JSON.parse(await readFile(file, 'utf8')) as T;
}

/** Crate description from its Cargo manifest, if any. */
async function cargoDescription(directory: string): Promise<string | undefined> {
  const text = await readFile(join(REPO_ROOT, directory, 'Cargo.toml'), 'utf8');

  return text.match(/^description\s*=\s*"(.*)"\s*$/m)?.[1];
}

async function buildProject(options: Options): Promise<ApiProject> {
  const packages: ApiPackage[] = [];

  if (options.only !== 'rust') {
    for (const config of TYPESCRIPT_PACKAGES) {
      const manifest = await readJson<{ version: string; description?: string }>(
        join(REPO_ROOT, config.directory, 'package.json'),
      );
      const result = await runTypeDoc(config);

      packages.push(
        normalizeTypeDoc({
          package: config,
          project: await readTypeDocJson(result.file),
          version: manifest.version,
          description: manifest.description ?? '',
        }),
      );
    }
  }

  if (options.only !== 'typescript') {
    const results = await runRustdoc(RUST_CRATES);

    for (const result of results) {
      const json = await readRustdocJson(result.file);
      const description =
        (await cargoDescription(result.crate.directory)) ??
        markdownToPlainText(firstParagraph(json.index[String(json.root)]?.docs ?? ''));

      packages.push(
        normalizeRustdoc({
          crate: result.crate,
          json,
          description,
          version: json.crate_version ?? '0.0.0',
        }),
      );
    }
  }

  return { packages };
}

async function main(): Promise<void> {
  const options = parseArgs(process.argv.slice(2));
  const started = Date.now();
  const ref = sourceRef();
  const project = await buildProject(options);
  const table = planSymbols(project);

  buildModuleEntries(table);

  const unresolved = resolveLinks(table);
  const built = buildPages(table, project, ref);
  const expected = project.packages.map((pkg) => pkg.name);
  const result = validate(table, built.files, unresolved, expected);

  for (const warning of result.warnings) console.warn(`warning: ${warning}`);

  if (result.errors.length > 0 || (options.strict && result.warnings.length > 0)) {
    for (const error of result.errors) console.error(`error: ${error}`);

    throw new Error(`API documentation validation failed with ${result.errors.length} error(s).`);
  }

  const manifest: ApiManifest = {
    schemaVersion: MANIFEST_SCHEMA_VERSION,
    generator: {
      typedoc: TYPEDOC_VERSION,
      rustdocFormat: RUSTDOC_FORMAT_VERSION,
      rustToolchain: RUSTDOC_TOOLCHAIN,
    },
    sourceRef: ref,
    pages: built.files.map((entry) => entry.page),
    folders: built.folders,
    packages: project.packages.map((pkg) => ({
      language: pkg.language,
      name: pkg.name,
      slug: pkg.slug,
      url: `/docs/api/${pkg.language}/${pkg.slug}/`,
      version: pkg.version,
      description: pkg.description,
    })),
    unresolved,
  };

  await rm(join(OUTPUT_DIR, 'pages'), { recursive: true, force: true });

  for (const { page, content } of built.files) {
    const file = join(OUTPUT_DIR, page.file);

    await mkdir(dirname(file), { recursive: true });
    await writeFile(file, JSON.stringify(content));
  }

  await writeFile(join(OUTPUT_DIR, 'manifest.json'), JSON.stringify(manifest));

  const symbols = built.files.filter((entry) => entry.page.type === 'symbol').length;

  console.log(
    `Generated ${built.files.length} API pages (${symbols} symbols) for ${project.packages.length} packages in ${((Date.now() - started) / 1000).toFixed(1)}s; ${unresolved.length} unlinked references.`,
  );
}

main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
});
