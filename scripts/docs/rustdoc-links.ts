import { readFileSync } from 'node:fs';
import { join } from 'node:path';

import { REPO_ROOT, RUST_CRATES, rustLibName, type RustCrateConfig } from './config.ts';
import type { ExternalRef, SymbolId } from './model.ts';

/** Rust namespaces used in symbol identities. */
export function rustNamespace(kind: string): string {
  switch (kind) {
    case 'function':
    case 'constant':
    case 'static':
    case 'assoc_const':
      return 'value';
    case 'macro':
    case 'proc_macro':
    case 'proc_attribute':
    case 'proc_derive':
      return 'macro';
    default:
      return 'type';
  }
}

/** Workspace crate (by library name such as `remedia_core`) to Cargo package name. */
export function workspaceCrateByLibName(
  libName: string,
  crates: RustCrateConfig[] = RUST_CRATES,
): string | undefined {
  return crates.find((crate) => rustLibName(crate.name) === libName)?.name;
}

let lockVersions: Map<string, string> | undefined;

/** Crate versions from Cargo.lock, parsed once. */
function cargoLockVersions(): Map<string, string> {
  if (lockVersions) return lockVersions;

  lockVersions = new Map();

  try {
    const text = readFileSync(join(REPO_ROOT, 'Cargo.lock'), 'utf8');
    let name: string | undefined;

    for (const line of text.split('\n')) {
      if (line.startsWith('name = ')) name = line.slice(8, -1);
      else if (line.startsWith('version = ') && name && !lockVersions.has(name)) {
        lockVersions.set(name, line.slice(11, -1));
      }
    }
  } catch {
    // Without Cargo.lock external crates are listed without links.
  }

  return lockVersions;
}

const STD_CRATES = new Set(['std', 'core', 'alloc', 'proc_macro', 'test']);

const KIND_PREFIX: Record<string, string> = {
  struct: 'struct',
  enum: 'enum',
  union: 'union',
  trait: 'trait',
  trait_alias: 'traitalias',
  type_alias: 'type',
  function: 'fn',
  constant: 'constant',
  static: 'static',
  macro: 'macro',
};

/**
 * Documentation location of an item defined outside the workspace.
 *
 * @param crateName Library name of the defining crate, for example `std` or `ffmpeg_next`.
 * @param path Definition path starting with the crate name.
 * @param kind rustdoc item kind.
 * @returns External reference with a URL when one can be built reliably.
 */
export function externalRust(crateName: string, path: string[], kind: string): ExternalRef {
  if (STD_CRATES.has(crateName)) {
    const name = path[path.length - 1] ?? '';

    if (kind === 'primitive') {
      return {
        package: crateName,
        url: `https://doc.rust-lang.org/stable/std/primitive.${name}.html`,
      };
    }

    const prefix = KIND_PREFIX[kind];

    if (kind === 'module') {
      return {
        package: crateName,
        url: `https://doc.rust-lang.org/stable/${path.join('/')}/index.html`,
      };
    }

    if (prefix) {
      return {
        package: crateName,
        url: `https://doc.rust-lang.org/stable/${path.slice(0, -1).join('/')}/${prefix}.${name}.html`,
      };
    }

    return { package: crateName, url: `https://doc.rust-lang.org/stable/${crateName}/` };
  }

  const packageName = [...cargoLockVersions().keys()].find(
    (candidate) => candidate.replaceAll('-', '_') === crateName,
  );
  const version = packageName ? cargoLockVersions().get(packageName) : undefined;

  if (packageName && version) {
    const name = path[path.length - 1] ?? '';

    return {
      package: packageName,
      url: `https://docs.rs/${packageName}/${version}/${crateName}/?search=${encodeURIComponent(name)}`,
    };
  }

  return { package: crateName };
}

/** Build a symbol identity from a Cargo package name and a rustdoc definition path. */
export function rustSymbolId(packageName: string, path: string[], kind: string): SymbolId {
  return {
    language: 'rust',
    package: packageName,
    file: '',
    path,
    namespace: rustNamespace(kind),
  };
}
