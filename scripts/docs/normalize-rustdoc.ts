import { readFile } from 'node:fs/promises';

import { RUST_CRATES, type RustCrateConfig } from './config.ts';
import { createNode, docText, emptyDocs, firstParagraph } from './docs-util.ts';
import {
  symbolKey,
  type ApiEntry,
  type ApiPackage,
  type Bound,
  type DocLink,
  type Docs,
  type ExternalRef,
  type Implementor,
  type ModuleEntry,
  type Parameter,
  type RustData,
  type RustImpl,
  type Signature,
  type SourceLocation,
  type SymbolId,
  type SymbolKind,
  type SymbolNode,
  type TypeExpr,
  type TypeParameter,
  type WherePredicate,
} from './model.ts';
import { externalRust, rustSymbolId, workspaceCrateByLibName } from './rustdoc-links.ts';
import { splitRustDocs } from './rustdoc-markdown.ts';
import type {
  RustBound,
  RustFnHeader,
  RustFnSig,
  RustGenericArgs,
  RustGenericParam,
  RustGenerics,
  RustId,
  RustPath,
  RustType,
  RustWherePredicate,
  RustdocCrate,
  RustdocItem,
} from './rustdoc-types.ts';
import { typeToText } from './text.ts';

/** Item kinds that receive their own documentation page. */
const PAGE_KINDS = new Set([
  'module',
  'struct',
  'enum',
  'union',
  'trait',
  'trait_alias',
  'function',
  'type_alias',
  'constant',
  'static',
  'macro',
]);

const SYMBOL_KIND: Record<string, SymbolKind> = {
  module: 'module',
  struct: 'struct',
  enum: 'enum',
  union: 'union',
  trait: 'trait',
  trait_alias: 'trait-alias',
  function: 'function',
  type_alias: 'type-alias',
  constant: 'constant',
  static: 'static',
  macro: 'macro',
};

const LANGUAGE_KIND: Record<string, string> = {
  module: 'Module',
  struct: 'Struct',
  enum: 'Enum',
  union: 'Union',
  trait: 'Trait',
  trait_alias: 'Trait alias',
  function: 'Function',
  type_alias: 'Type alias',
  constant: 'Constant',
  static: 'Static',
  macro: 'Macro',
};

/** Inputs for converting one rustdoc JSON crate. */
export interface NormalizeRustdocOptions {
  crate: RustCrateConfig;
  /** Crates whose items can be linked across crates. Defaults to the repository's documented crates. */
  workspaceCrates?: RustCrateConfig[];
  json: RustdocCrate;
  description: string;
  version: string;
}

/** Read a raw rustdoc JSON file. */
export async function readRustdocJson(file: string): Promise<RustdocCrate> {
  return JSON.parse(await readFile(file, 'utf8')) as RustdocCrate;
}

/**
 * Convert rustdoc JSON to the shared symbol model.
 *
 * @param options Crate configuration and parsed rustdoc output.
 * @returns A package with a single entry whose root is the crate module.
 */
export function normalizeRustdoc(options: NormalizeRustdocOptions): ApiPackage {
  return new RustdocNormalizer(options).run();
}

/** Kind of an item: the single key of `inner`. */
function kindOf(item: RustdocItem): string {
  return Object.keys(item.inner)[0] ?? 'unknown';
}

function innerOf<T>(item: RustdocItem): T {
  return item.inner[kindOf(item)] as T;
}

interface UseInner {
  source: string;
  name: string;
  id: RustId | null;
  is_glob: boolean;
}

interface ModuleInner {
  is_crate: boolean;
  items: RustId[];
  is_stripped: boolean;
}

interface ImplInner {
  is_unsafe: boolean;
  generics: RustGenerics;
  trait: RustPath | null;
  for: RustType;
  items: RustId[];
  is_negative: boolean;
  is_synthetic: boolean;
  blanket_impl: RustType | null;
}

class RustdocNormalizer {
  readonly #crate: RustCrateConfig;
  readonly #json: RustdocCrate;
  readonly #description: string;
  readonly #version: string;
  readonly #workspace: RustCrateConfig[];
  /** Item id to canonical public path (without crate name). */
  readonly #canonical = new Map<RustId, string[]>();
  /** Item id to additional public paths. */
  readonly #aliasPaths = new Map<RustId, string[][]>();
  /** Module path (joined) to re-export entries and the id of the item they alias. */
  readonly #aliases = new Map<string, { entry: ModuleEntry; id: RustId | undefined }[]>();
  /** Item id to model key, for documented items and members. */
  readonly #keys = new Map<RustId, string>();
  /** Impl id to its page anchor. */
  readonly #implAnchors = new Map<RustId, string>();
  /** Impl ids that belong to a documented type. */
  #currentLinks: Record<string, DocLink> = {};

  constructor(options: NormalizeRustdocOptions) {
    this.#crate = options.crate;
    this.#json = options.json;
    this.#description = options.description;
    this.#version = options.version;
    this.#workspace = options.workspaceCrates ?? RUST_CRATES;
  }

  run(): ApiPackage {
    this.#computeCanonical();
    this.#assignKeys();
    this.#assignImplAnchors();

    const rootItem = this.#item(this.#json.root);
    const root = this.#moduleNode(rootItem, []);

    root.name = this.#crate.name;
    root.languageKind = 'Crate';

    const entry: ApiEntry = {
      subpath: '.',
      slug: '',
      title: this.#crate.name,
      description: this.#description,
      generated: false,
      root,
    };

    return {
      language: 'rust',
      name: this.#crate.name,
      slug: this.#crate.slug,
      version: this.#json.crate_version ?? this.#version,
      description: this.#description,
      directory: this.#crate.directory,
      entries: [entry],
    };
  }

  // ---- lookup helpers ----

  #item(id: RustId): RustdocItem {
    const item = this.#json.index[String(id)];

    if (!item) throw new Error(`${this.#crate.name}: rustdoc item ${id} is missing.`);

    return item;
  }

  #maybeItem(id: RustId | null | undefined): RustdocItem | undefined {
    return id === null || id === undefined ? undefined : this.#json.index[String(id)];
  }

  #isPublic(item: RustdocItem): boolean {
    return item.visibility === 'public';
  }

  // ---- canonical paths ----

  /**
   * Choose one public path for every documented item, following rustdoc's rule: an item lives where
   * it is defined if that module is public, otherwise where it is first re-exported (shortest path wins).
   */
  #computeCanonical(): void {
    const uses: { modulePath: string[]; item: RustdocItem }[] = [];
    const walk = (moduleId: RustId, path: string[]) => {
      const moduleItem = this.#item(moduleId);

      for (const id of innerOf<ModuleInner>(moduleItem).items) {
        const item = this.#maybeItem(id);

        if (!item) continue;

        const kind = kindOf(item);

        if (kind === 'use') {
          uses.push({ modulePath: path, item });
          continue;
        }

        if (!PAGE_KINDS.has(kind) || !this.#isPublic(item) || !item.name) continue;

        const next = [...path, item.name];

        this.#canonical.set(id, next);

        if (kind === 'module') walk(id, next);
      }
    };

    walk(this.#json.root, []);

    uses.sort((a, b) => a.modulePath.length - b.modulePath.length);

    for (const { modulePath, item } of uses) {
      const use = innerOf<UseInner>(item);

      if (use.is_glob || use.id === null || !this.#isPublic(item)) continue;

      const target = this.#maybeItem(use.id);
      const parent = modulePath.join('::');

      if (!target) {
        this.#addAlias(
          parent,
          {
            name: use.name,
            kind: 'type-alias',
            target: this.#externalKey(use.id) ?? '',
            reexport: true,
          },
          undefined,
        );
        continue;
      }

      const kind = kindOf(target);

      if (!PAGE_KINDS.has(kind)) continue;

      if (!this.#canonical.has(use.id)) {
        this.#canonical.set(use.id, [...modulePath, use.name]);

        if (kind === 'module') this.#walkReexportedModule(use.id, [...modulePath, use.name]);

        continue;
      }

      const canonical = this.#canonical.get(use.id) ?? [];

      if (canonical.join('::') !== [...modulePath, use.name].join('::')) {
        this.#aliasPaths.set(use.id, [
          ...(this.#aliasPaths.get(use.id) ?? []),
          [...modulePath, use.name],
        ]);
        this.#addAlias(
          parent,
          { name: use.name, kind: SYMBOL_KIND[kind] ?? 'type-alias', target: '', reexport: true },
          use.id,
        );
      }
    }
  }

  #walkReexportedModule(moduleId: RustId, path: string[]): void {
    const moduleItem = this.#item(moduleId);

    for (const id of innerOf<ModuleInner>(moduleItem).items) {
      const item = this.#maybeItem(id);

      if (!item || !item.name || !PAGE_KINDS.has(kindOf(item)) || !this.#isPublic(item)) continue;

      if (!this.#canonical.has(id)) {
        this.#canonical.set(id, [...path, item.name]);

        if (kindOf(item) === 'module') this.#walkReexportedModule(id, [...path, item.name]);
      }
    }
  }

  #addAlias(parent: string, entry: ModuleEntry, id: RustId | undefined): void {
    const list = this.#aliases.get(parent) ?? [];

    list.push({ entry, id });
    this.#aliases.set(parent, list);
  }

  // ---- identity ----

  /** Definition path used in identities: stable regardless of re-exports. */
  #definition(id: RustId): { path: string[]; kind: string } | undefined {
    const entry = this.#json.paths[String(id)];

    if (entry && entry.crate_id === 0) return { path: entry.path.slice(1), kind: entry.kind };

    const canonical = this.#canonical.get(id);
    const item = this.#maybeItem(id);

    return canonical && item ? { path: canonical, kind: kindOf(item) } : undefined;
  }

  #idFor(id: RustId): SymbolId | undefined {
    const definition = this.#definition(id);

    return definition
      ? rustSymbolId(this.#crate.name, definition.path, definition.kind)
      : undefined;
  }

  /** Key of a possibly-foreign item id when it belongs to a workspace crate. */
  #externalKey(id: RustId): string | undefined {
    const entry = this.#json.paths[String(id)];

    if (!entry || entry.crate_id === 0) return undefined;

    const crate = this.#json.external_crates[String(entry.crate_id)]?.name;
    const workspace = crate ? workspaceCrateByLibName(crate, this.#workspace) : undefined;

    if (!crate || !workspace) return undefined;

    return symbolKey(rustSymbolId(workspace, entry.path.slice(1), entry.kind));
  }

  /** Walk documented items once to assign keys to every item that can be linked to. */
  #assignKeys(): void {
    for (const [id] of this.#canonical) {
      const symbolId = this.#idFor(id);

      if (!symbolId) continue;

      const key = symbolKey(symbolId);
      const item = this.#item(id);

      this.#keys.set(id, key);
      this.#assignMemberKeys(item, symbolId);
    }
  }

  #assignMemberKeys(item: RustdocItem, parent: SymbolId): void {
    const kind = kindOf(item);
    const child = (id: RustId | null | undefined, namespace: string) => {
      const member = this.#maybeItem(id);

      if (!member || !member.name) return;

      this.#keys.set(
        member.id,
        symbolKey({ ...parent, path: [...parent.path, member.name], namespace }),
      );
    };

    if (kind === 'struct' || kind === 'union') {
      const data = innerOf<{ kind?: unknown; fields?: RustId[] }>(item);

      for (const id of this.#structFieldIds(
        kind === 'union' ? { plain: { fields: data.fields ?? [] } } : data.kind,
      )) {
        child(id, 'field');
      }
    } else if (kind === 'enum') {
      for (const variantId of innerOf<{ variants: RustId[] }>(item).variants) {
        child(variantId, 'variant');

        const variant = this.#maybeItem(variantId);

        if (!variant?.name) continue;

        const variantParent = {
          ...parent,
          path: [...parent.path, variant.name],
          namespace: 'variant',
        };
        const kindInfo = innerOf<{ kind: unknown }>(variant).kind;

        for (const fieldId of this.#structFieldIds(
          kindInfo === 'plain' ? { plain: { fields: [] } } : kindInfo,
        )) {
          const field = this.#maybeItem(fieldId);

          if (field?.name) {
            this.#keys.set(
              field.id,
              symbolKey({
                ...variantParent,
                path: [...variantParent.path, field.name],
                namespace: 'field',
              }),
            );
          }
        }
      }
    } else if (kind === 'trait') {
      for (const id of innerOf<{ items: RustId[] }>(item).items) child(id, 'assoc');
    }

    if (kind === 'struct' || kind === 'enum' || kind === 'union') {
      for (const implId of innerOf<{ impls: RustId[] }>(item).impls) {
        const impl = this.#maybeItem(implId);

        if (!impl) continue;

        for (const itemId of innerOf<ImplInner>(impl).items) {
          const member = this.#maybeItem(itemId);

          if (member?.name) {
            this.#keys.set(
              itemId,
              symbolKey({
                ...parent,
                path: [...parent.path, `impl#${implId}`, member.name],
                namespace: 'impl',
              }),
            );
          }
        }
      }
    }
  }

  /** Field ids of a struct/variant shape: tuple, plain, or struct-like. */
  #structFieldIds(shape: unknown): RustId[] {
    if (!shape || typeof shape === 'string') return [];

    const value = shape as {
      tuple?: (RustId | null)[];
      plain?: { fields: RustId[] };
      struct?: { fields: RustId[] };
    };

    if (value.tuple) return value.tuple.filter((id): id is RustId => id !== null);

    return value.plain?.fields ?? value.struct?.fields ?? [];
  }

  /** Give every impl block a stable, unique anchor derived from its trait and self type. */
  #assignImplAnchors(): void {
    const seen = new Map<string, number>();
    const impls = Object.values(this.#json.index)
      .filter((item) => kindOf(item) === 'impl')
      .sort((a, b) => a.id - b.id);

    for (const item of impls) {
      const impl = innerOf<ImplInner>(item);
      const head = impl.trait ? typeToText(this.#path(impl.trait)) : 'inherent';
      const raw = `impl-${head}-for-${typeToText(this.#type(impl.for))}`;
      const base = raw.replace(/[^A-Za-z0-9_]+/g, '-').replace(/^-+|-+$/g, '') || 'impl';
      const count = seen.get(base) ?? 0;

      seen.set(base, count + 1);
      this.#implAnchors.set(item.id, count === 0 ? base : `${base}-${count + 1}`);
    }
  }

  // ---- modules and items ----

  #moduleNode(item: RustdocItem, path: string[]): SymbolNode {
    const definition = this.#idFor(item.id);
    const id: SymbolId =
      definition ??
      ({
        language: 'rust',
        package: this.#crate.name,
        file: '',
        path,
        namespace: 'type',
      } satisfies SymbolId);
    const node = createNode({
      key: symbolKey(id),
      id,
      name: item.name ?? this.#crate.name,
      kind: 'module',
      languageKind: 'Module',
      language: 'rust',
    });

    node.docs = this.#docs(item);
    this.#applySource(node, item);
    node.rust = this.#rustData(item);

    const children = [...this.#canonical.entries()]
      .filter(([, childPath]) => childPath.slice(0, -1).join('::') === path.join('::'))
      .map(([childId]) => this.#item(childId))
      .sort(compareItems);

    for (const child of children) {
      const symbol =
        kindOf(child) === 'module'
          ? this.#moduleNode(child, this.#canonical.get(child.id) ?? [])
          : this.#itemNode(child);

      if (symbol) node.members.push(symbol);
    }

    node.entries = (this.#aliases.get(path.join('::')) ?? [])
      .map(({ entry, id: aliasId }) => ({
        ...entry,
        target: aliasId === undefined ? entry.target : (this.#keys.get(aliasId) ?? ''),
      }))
      .filter((entry) => entry.target)
      .sort((a, b) => a.name.localeCompare(b.name));

    return node;
  }

  #itemNode(item: RustdocItem): SymbolNode | undefined {
    const kind = kindOf(item);
    const symbolId = this.#idFor(item.id);

    if (!symbolId || !item.name) return undefined;

    const node = createNode({
      key: symbolKey(symbolId),
      id: symbolId,
      name: item.name,
      kind: SYMBOL_KIND[kind] ?? 'struct',
      languageKind: LANGUAGE_KIND[kind] ?? kind,
      language: 'rust',
    });

    node.docs = this.#docs(item);
    this.#applySource(node, item);
    node.rust = this.#rustData(item);

    const alias = this.#aliasPaths.get(item.id);

    if (alias) node.rust.reexportedAs = alias.map((path) => path.join('::'));

    switch (kind) {
      case 'struct':
      case 'union':
        this.#structLike(node, item, kind);
        break;
      case 'enum':
        this.#enumNode(node, item);
        break;
      case 'trait':
        this.#traitNode(node, item);
        break;
      case 'function':
        this.#applyFunction(node, item);
        break;
      case 'type_alias': {
        const data = innerOf<{ type: RustType; generics: RustGenerics }>(item);

        this.#applyGenerics(node, data.generics);
        node.type = this.#type(data.type);
        break;
      }
      case 'constant': {
        const data = innerOf<{ type: RustType; const: { expr: string; value: string | null } }>(
          item,
        );

        node.type = this.#type(data.type);
        node.value = data.const.value ?? data.const.expr;
        break;
      }
      case 'static': {
        const data = innerOf<{ type: RustType; is_mutable: boolean; expr: string }>(item);

        node.type = this.#type(data.type);

        if (data.is_mutable) node.flags.push('mut');

        if (data.expr && data.expr.length < 120) node.value = data.expr;
        break;
      }
      case 'macro':
        node.value = innerOf<string>(item);
        break;
      default:
        break;
    }

    return node;
  }

  #applySource(node: SymbolNode, item: RustdocItem): void {
    const source = this.#span(item);

    if (source) node.source = source;
  }

  #span(item: RustdocItem): SourceLocation | undefined {
    if (!item.span || item.span.filename.startsWith('/')) return undefined;

    return {
      file: item.span.filename.replaceAll('\\', '/'),
      line: item.span.begin[0],
      endLine: item.span.end[0],
    };
  }

  #rustData(item: RustdocItem): RustData {
    const attributes: string[] = [];

    for (const attribute of item.attrs) {
      const text = attributeText(attribute);

      if (text) attributes.push(text);
    }

    return {
      visibility:
        item.visibility === 'public' ? 'pub' : item.visibility === 'default' ? '' : 'pub(crate)',
      traitFlags: [],
      attributes,
      impls: [],
      implementors: [],
      bounds: [],
      reexportedAs: [],
    };
  }

  // ---- structs, enums, unions ----

  #structLike(node: SymbolNode, item: RustdocItem, kind: string): void {
    const data = innerOf<{
      kind?: unknown;
      fields?: RustId[];
      has_stripped_fields?: boolean;
      generics: RustGenerics;
      impls: RustId[];
    }>(item);

    this.#applyGenerics(node, data.generics);

    const shape =
      kind === 'union'
        ? {
            plain: {
              fields: data.fields ?? [],
              has_stripped_fields: data.has_stripped_fields ?? false,
            },
          }
        : data.kind;

    if (node.rust) {
      node.rust.structKind =
        shape === 'unit' ? 'unit' : (shape as { tuple?: unknown }).tuple ? 'tuple' : 'plain';

      const plain = (shape as { plain?: { has_stripped_fields?: boolean } }).plain;

      if (plain?.has_stripped_fields) node.rust.hasStrippedFields = true;

      if ((shape as { tuple?: (RustId | null)[] }).tuple?.some((id) => id === null)) {
        node.rust.hasStrippedFields = true;
      }
    }

    this.#fields(node, this.#structFieldIds(shape), 'field');
    this.#attachImpls(node, data.impls);
  }

  #fields(node: SymbolNode, ids: RustId[], prefix: string, inheritVisibility = false): void {
    for (const id of ids) {
      const field = this.#maybeItem(id);

      // Enum variant fields have no visibility of their own: they are as public as the variant.
      if (!field || (!inheritVisibility && !this.#isPublic(field))) continue;

      const name = field.name ?? String(node.members.length);
      const key =
        this.#keys.get(id) ??
        symbolKey({ ...node.id, path: [...node.id.path, name], namespace: 'field' });
      const member = createNode({
        key,
        id: { ...node.id, path: [...node.id.path, name], namespace: 'field' },
        name,
        kind: 'field',
        languageKind: 'Field',
        language: 'rust',
        anchor: `${prefix}-${name}`,
      });

      member.docs = this.#docs(field);
      member.type = this.#type(innerOf<RustType>(field));
      this.#applySource(member, field);
      node.members.push(member);
    }
  }

  #enumNode(node: SymbolNode, item: RustdocItem): void {
    const data = innerOf<{
      generics: RustGenerics;
      variants: RustId[];
      has_stripped_variants: boolean;
      impls: RustId[];
    }>(item);

    this.#applyGenerics(node, data.generics);

    if (node.rust && data.has_stripped_variants) node.rust.hasStrippedFields = true;

    for (const variantId of data.variants) {
      const variant = this.#item(variantId);
      const name = variant.name ?? '';
      const kindInfo = innerOf<{
        kind: unknown;
        discriminant: { expr: string; value: string } | null;
      }>(variant);
      const member = createNode({
        key:
          this.#keys.get(variantId) ??
          symbolKey({ ...node.id, path: [...node.id.path, name], namespace: 'variant' }),
        id: { ...node.id, path: [...node.id.path, name], namespace: 'variant' },
        name,
        kind: 'variant',
        languageKind: 'Variant',
        language: 'rust',
        anchor: `variant-${name}`,
      });

      member.docs = this.#docs(variant);
      member.rust = this.#rustData(variant);
      member.rust.structKind =
        kindInfo.kind === 'plain'
          ? 'unit'
          : (kindInfo.kind as { tuple?: unknown }).tuple
            ? 'tuple'
            : 'plain';

      if (kindInfo.discriminant) member.rust.discriminant = kindInfo.discriminant.expr;

      this.#applySource(member, variant);
      this.#fields(
        member,
        this.#structFieldIds(kindInfo.kind === 'plain' ? { plain: { fields: [] } } : kindInfo.kind),
        `variant-${name}.field`,
        true,
      );
      node.members.push(member);
    }

    this.#attachImpls(node, data.impls);
  }

  // ---- traits ----

  #traitNode(node: SymbolNode, item: RustdocItem): void {
    const data = innerOf<{
      is_auto: boolean;
      is_unsafe: boolean;
      is_dyn_compatible: boolean;
      items: RustId[];
      generics: RustGenerics;
      bounds: RustBound[];
      implementations: RustId[];
    }>(item);

    this.#applyGenerics(node, data.generics);

    const rust = node.rust as RustData;

    if (data.is_auto) rust.traitFlags.push('auto');

    if (data.is_unsafe) rust.traitFlags.push('unsafe');

    if (!data.is_dyn_compatible) rust.traitFlags.push('not dyn compatible');

    rust.bounds = this.#bounds(data.bounds);

    for (const id of data.items) {
      const member = this.#associatedItem(this.#item(id), node, 'trait');

      if (member) node.members.push(member);
    }

    const seen = new Set<string>();

    for (const implId of data.implementations) {
      const impl = this.#maybeItem(implId);

      if (!impl) continue;

      const inner = innerOf<ImplInner>(impl);
      const forType = this.#type(inner.for);
      const implKind: Implementor['implKind'] = inner.is_synthetic
        ? 'auto'
        : inner.blanket_impl
          ? 'blanket'
          : 'trait';
      const text = `${implKind}:${typeToText(forType)}`;

      if (seen.has(text)) continue;

      seen.add(text);

      const implementor: Implementor = { forType, implKind };
      const anchor = this.#implAnchors.get(implId);
      const target = forType.kind === 'reference' ? forType.target : undefined;

      if (anchor && target) {
        implementor.anchor = anchor;
        implementor.target = target;
      } else if (target) {
        implementor.target = target;
      }

      rust.implementors.push(implementor);
    }
  }

  /** Method, associated type or associated constant inside a trait or impl. */
  #associatedItem(
    item: RustdocItem,
    owner: SymbolNode,
    scope: 'trait' | 'impl',
    implAnchor?: string,
  ): SymbolNode | undefined {
    const kind = kindOf(item);
    const name = item.name;

    if (!name) return undefined;

    const base = scope === 'impl' && implAnchor ? `${implAnchor}.` : '';
    const key =
      this.#keys.get(item.id) ??
      symbolKey({
        ...owner.id,
        path: [...owner.id.path, scope === 'impl' ? `impl#${item.id}` : name, name],
        namespace: scope === 'impl' ? 'impl' : 'assoc',
      });
    const id: SymbolId = {
      ...owner.id,
      path: [...owner.id.path, name],
      namespace: scope === 'impl' ? 'impl' : 'assoc',
    };
    let node: SymbolNode;

    switch (kind) {
      case 'function': {
        node = createNode({
          key,
          id,
          name,
          kind: 'method',
          languageKind: 'Method',
          language: 'rust',
          anchor: `${base}method-${name}`,
        });
        this.#applyFunction(node, item);
        break;
      }
      case 'assoc_type': {
        const data = innerOf<{
          generics: RustGenerics;
          bounds: RustBound[];
          type: RustType | null;
        }>(item);

        node = createNode({
          key,
          id,
          name,
          kind: 'associated-type',
          languageKind: 'Associated type',
          language: 'rust',
          anchor: `${base}associatedtype-${name}`,
        });
        this.#applyGenerics(node, data.generics);

        if (data.type) node.type = this.#type(data.type);

        node.rust = this.#rustData(item);
        node.rust.bounds = this.#bounds(data.bounds);
        break;
      }
      case 'assoc_const': {
        const data = innerOf<{ type: RustType; value: string | null }>(item);

        node = createNode({
          key,
          id,
          name,
          kind: 'associated-constant',
          languageKind: 'Associated constant',
          language: 'rust',
          anchor: `${base}associatedconstant-${name}`,
        });
        node.type = this.#type(data.type);

        if (data.value) node.value = data.value;

        break;
      }
      default:
        return undefined;
    }

    node.docs = this.#docs(item);
    node.rust ??= this.#rustData(item);
    this.#applySource(node, item);

    if (kind === 'function' && scope === 'trait') {
      node.rust.provided = innerOf<{ has_body: boolean }>(item).has_body;
    }

    return node;
  }

  // ---- impls ----

  #attachImpls(node: SymbolNode, implIds: RustId[]): void {
    const rust = node.rust as RustData;
    const anchors = new Set<string>();

    for (const implId of implIds) {
      const item = this.#maybeItem(implId);

      if (!item) continue;

      const impl = innerOf<ImplInner>(item);
      const implKind: RustImpl['implKind'] = impl.trait
        ? impl.is_synthetic
          ? 'auto'
          : impl.blanket_impl
            ? 'blanket'
            : 'trait'
        : 'inherent';
      const anchor =
        implKind === 'inherent'
          ? `impl-${rust.impls.filter((existing) => existing.implKind === 'inherent').length || ''}`.replace(
              /-$/,
              '',
            )
          : (this.#implAnchors.get(implId) ?? `impl-${implId}`);

      if (anchors.has(anchor)) continue;

      anchors.add(anchor);

      const result: RustImpl = {
        anchor,
        implKind,
        forType: this.#type(impl.for),
        typeParameters: [],
        whereClauses: [],
        negative: impl.is_negative,
        unsafe: impl.is_unsafe,
        docs: this.#docs(item),
        items: [],
      };

      if (impl.trait) result.trait = this.#path(impl.trait);

      const generics = this.#generics(impl.generics);

      result.typeParameters = generics.parameters;
      result.whereClauses = generics.where;

      const source = this.#span(item);

      if (source) result.source = source;

      for (const itemId of impl.items) {
        const member = this.#maybeItem(itemId);

        if (!member || (implKind === 'inherent' && !this.#isPublic(member))) continue;

        const converted = this.#associatedItem(member, node, 'impl', anchor);

        if (converted) result.items.push(converted);
      }

      rust.impls.push(result);
    }
  }

  // ---- functions and generics ----

  #applyFunction(node: SymbolNode, item: RustdocItem): void {
    const data = innerOf<{ sig: RustFnSig; generics: RustGenerics; header: RustFnHeader }>(item);
    const signature = this.#signature(item.name ?? node.name, data.sig, data.generics, data.header);

    signature.docs = emptyDocs();
    node.signatures = [signature];
    node.rust ??= this.#rustData(item);
  }

  #signature(
    name: string,
    sig: RustFnSig,
    generics: RustGenerics,
    header: RustFnHeader,
  ): Signature {
    const converted = this.#generics(generics);
    const parameters: Parameter[] = [];
    let receiver: string | undefined;

    sig.inputs.forEach(([parameterName, type], index) => {
      if (index === 0 && parameterName === 'self') {
        receiver = receiverText(type) ?? undefined;

        if (receiver) return;
      }

      parameters.push({
        name: parameterName,
        type: this.#type(type),
        optional: false,
        rest: false,
      });
    });

    const qualifiers: string[] = [];

    if (header.is_const) qualifiers.push('const');

    if (header.is_async) qualifiers.push('async');

    if (header.is_unsafe) qualifiers.push('unsafe');

    if (header.abi !== 'Rust') qualifiers.push(`extern "${abiName(header.abi)}"`);

    const result: Signature = {
      name,
      signatureKind: 'fn',
      typeParameters: converted.parameters,
      parameters,
      whereClauses: converted.where,
      qualifiers,
      docs: emptyDocs(),
    };

    if (receiver) result.receiver = receiver;

    if (
      sig.output &&
      !(typeof sig.output === 'object' && 'tuple' in sig.output && sig.output.tuple.length === 0)
    ) {
      result.returns = { type: this.#type(sig.output) };
    }

    return result;
  }

  #applyGenerics(node: SymbolNode, generics: RustGenerics): void {
    const converted = this.#generics(generics);

    node.typeParameters = converted.parameters;
    node.whereClauses = converted.where;
  }

  #generics(generics: RustGenerics): { parameters: TypeParameter[]; where: WherePredicate[] } {
    const parameters: TypeParameter[] = [];

    for (const param of generics.params) {
      const converted = this.#genericParam(param);

      if (converted) parameters.push(converted);
    }

    return {
      parameters,
      where: generics.where_predicates.map((predicate) => this.#wherePredicate(predicate)),
    };
  }

  #genericParam(param: RustGenericParam): TypeParameter | undefined {
    const kind = param.kind;

    if ('lifetime' in kind) {
      return {
        name: param.name,
        paramKind: 'lifetime',
        bounds: kind.lifetime.outlives.map((lifetime) => ({ kind: 'outlives', lifetime }) as Bound),
        modifiers: [],
      };
    }

    if ('type' in kind) {
      if (kind.type.is_synthetic) return undefined;

      const result: TypeParameter = {
        name: param.name,
        paramKind: 'type',
        bounds: this.#bounds(kind.type.bounds),
        modifiers: [],
      };

      if (kind.type.default) result.default = this.#type(kind.type.default);

      return result;
    }

    const result: TypeParameter = {
      name: param.name,
      paramKind: 'const',
      bounds: [],
      modifiers: [],
      constType: this.#type(kind.const.type),
    };

    if (kind.const.default) result.default = { kind: 'literal', value: kind.const.default };

    return result;
  }

  #wherePredicate(predicate: RustWherePredicate): WherePredicate {
    if ('bound_predicate' in predicate) {
      return {
        subject: this.#type(predicate.bound_predicate.type),
        bounds: this.#bounds(predicate.bound_predicate.bounds),
      };
    }

    if ('lifetime_predicate' in predicate) {
      return {
        subject: { kind: 'lifetime', name: predicate.lifetime_predicate.lifetime },
        bounds: predicate.lifetime_predicate.outlives.map(
          (lifetime) => ({ kind: 'outlives', lifetime }) as Bound,
        ),
      };
    }

    const equality =
      'type' in predicate.eq_predicate.rhs
        ? this.#type(predicate.eq_predicate.rhs.type)
        : ({ kind: 'unknown', text: '_' } as TypeExpr);

    return {
      subject: this.#type(predicate.eq_predicate.lhs),
      bounds: [{ kind: 'trait', trait: equality, modifier: '= ' }],
    };
  }

  #bounds(bounds: RustBound[]): Bound[] {
    const result: Bound[] = [];

    for (const bound of bounds) {
      if ('outlives' in bound) {
        result.push({ kind: 'outlives', lifetime: bound.outlives });
      } else if ('trait_bound' in bound) {
        const converted: Extract<Bound, { kind: 'trait' }> = {
          kind: 'trait',
          trait: this.#path(bound.trait_bound.trait),
        };

        if (bound.trait_bound.modifier === 'maybe') converted.modifier = '?';
        else if (bound.trait_bound.modifier === 'maybe_const') converted.modifier = '~const ';

        if (bound.trait_bound.generic_params.length > 0) {
          converted.higherRanked = bound.trait_bound.generic_params.map((param) => param.name);
        }

        result.push(converted);
      }
    }

    return result;
  }

  // ---- types ----

  #path(path: RustPath): Extract<TypeExpr, { kind: 'reference' }> {
    const reference: Extract<TypeExpr, { kind: 'reference' }> = {
      kind: 'reference',
      name: path.path,
    };
    const resolved = this.#referenceTarget(path.id);

    if (resolved.target) reference.target = resolved.target;

    if (resolved.external) reference.external = resolved.external;

    if (!resolved.target && !resolved.external) reference.qualified = `private:${path.path}`;

    if (path.args) this.#applyArgs(reference, path.args);

    return reference;
  }

  #applyArgs(reference: Extract<TypeExpr, { kind: 'reference' }>, args: RustGenericArgs): void {
    if (args.angle_bracketed) {
      const converted: TypeExpr[] = [];

      for (const arg of args.angle_bracketed.args) {
        if (arg === 'infer') converted.push({ kind: 'placeholder' });
        else if ('lifetime' in arg) converted.push({ kind: 'intrinsic', name: arg.lifetime });
        else if ('type' in arg) converted.push(this.#type(arg.type));
        else converted.push({ kind: 'literal', value: arg.const.expr });
      }

      if (converted.length > 0) reference.args = converted;

      if (args.angle_bracketed.constraints.length > 0) {
        reference.bindings = args.angle_bracketed.constraints.map((constraint) => {
          if ('equality' in constraint.binding) {
            const equality = constraint.binding.equality;

            return 'type' in equality
              ? { name: constraint.name, type: this.#type(equality.type) }
              : { name: constraint.name, type: { kind: 'unknown', text: '_' } as TypeExpr };
          }

          return { name: constraint.name, bounds: this.#bounds(constraint.binding.constraint) };
        });
      }
    } else if (args.parenthesized) {
      reference.fnSugar = true;
      reference.args = args.parenthesized.inputs.map((input) => this.#type(input));

      if (args.parenthesized.output) {
        reference.bindings = [{ name: 'Output', type: this.#type(args.parenthesized.output) }];
      }
    }
  }

  /** Resolve an item id used in a type or link to a documented symbol or an external reference. */
  #referenceTarget(id: RustId): { target?: string; external?: ExternalRef } {
    const local = this.#keys.get(id);

    if (local) return { target: local };

    const workspace = this.#externalKey(id);

    if (workspace) return { target: workspace };

    const entry = this.#json.paths[String(id)];

    if (!entry || entry.crate_id === 0) return {};

    const crate = this.#json.external_crates[String(entry.crate_id)]?.name;

    return crate ? { external: externalRust(crate, entry.path, entry.kind) } : {};
  }

  /** Key of an associated item of a trait defined in another workspace crate, built from the trait's identity. */
  #foreignAssociatedKey(traitId: RustId, name: string): string | undefined {
    const entry = this.#json.paths[String(traitId)];

    if (!entry || entry.crate_id === 0) return undefined;

    const crate = this.#json.external_crates[String(entry.crate_id)]?.name;
    const workspace = crate ? workspaceCrateByLibName(crate, this.#workspace) : undefined;

    if (!workspace) return undefined;

    const id = rustSymbolId(workspace, entry.path.slice(1), entry.kind);

    return symbolKey({ ...id, path: [...id.path, name], namespace: 'assoc' });
  }

  #type(type: RustType): TypeExpr {
    if (type === 'infer') return { kind: 'placeholder' };

    if ('resolved_path' in type) return this.#path(type.resolved_path);

    if ('generic' in type) return { kind: 'type-parameter', name: type.generic };

    if ('primitive' in type) return { kind: 'intrinsic', name: type.primitive };

    if ('tuple' in type) {
      return type.tuple.length === 0
        ? { kind: 'intrinsic', name: '()' }
        : { kind: 'tuple', elements: type.tuple.map((member) => this.#type(member)) };
    }

    if ('slice' in type) return { kind: 'slice', element: this.#type(type.slice) };

    if ('array' in type)
      return { kind: 'fixed-array', element: this.#type(type.array.type), length: type.array.len };

    if ('pat' in type) return this.#type(type.pat.type);

    if ('impl_trait' in type) return { kind: 'impl-trait', bounds: this.#bounds(type.impl_trait) };

    if ('raw_pointer' in type) {
      return {
        kind: 'pointer',
        mutable: type.raw_pointer.is_mutable,
        type: this.#type(type.raw_pointer.type),
      };
    }

    if ('borrowed_ref' in type) {
      const borrowed: Extract<TypeExpr, { kind: 'borrowed' }> = {
        kind: 'borrowed',
        mutable: type.borrowed_ref.is_mutable,
        type: this.#type(type.borrowed_ref.type),
      };

      if (type.borrowed_ref.lifetime) borrowed.lifetime = type.borrowed_ref.lifetime;

      return borrowed;
    }

    if ('qualified_path' in type) return this.#qualifiedPath(type.qualified_path);

    if ('dyn_trait' in type) {
      const dyn: Extract<TypeExpr, { kind: 'dyn-trait' }> = {
        kind: 'dyn-trait',
        bounds: type.dyn_trait.traits.map((entry): Bound => {
          const bound: Extract<Bound, { kind: 'trait' }> = {
            kind: 'trait',
            trait: this.#path(entry.trait),
          };

          if (entry.generic_params.length > 0)
            bound.higherRanked = entry.generic_params.map((param) => param.name);

          return bound;
        }),
      };

      if (type.dyn_trait.lifetime) dyn.lifetime = type.dyn_trait.lifetime;

      return dyn;
    }

    if ('function_pointer' in type) {
      const pointer = type.function_pointer;
      const signature = this.#signature(
        'fn',
        pointer.sig,
        { params: pointer.generic_params, where_predicates: [] },
        pointer.header,
      );

      return { kind: 'function', constructor: false, signature };
    }

    return { kind: 'unknown', text: '?' };
  }

  #qualifiedPath(path: Extract<RustType, { qualified_path: unknown }>['qualified_path']): TypeExpr {
    const result: Extract<TypeExpr, { kind: 'qualified-path' }> = {
      kind: 'qualified-path',
      self: this.#type(path.self_type),
      name: path.name,
    };

    if (path.trait) {
      const trait = this.#path(path.trait);

      result.trait = trait;

      if (trait.target) {
        const traitItem = this.#maybeItem(path.trait.id);
        const assoc = traitItem
          ? innerOf<{ items?: RustId[] }>(traitItem).items?.find(
              (itemId) => this.#maybeItem(itemId)?.name === path.name,
            )
          : undefined;
        const key =
          assoc === undefined
            ? this.#foreignAssociatedKey(path.trait.id, path.name)
            : this.#keys.get(assoc);

        if (key) result.target = key;
      }
    }

    return result;
  }

  // ---- documentation ----

  #docs(item: RustdocItem): Docs {
    const docs = emptyDocs();

    if (item.deprecation) {
      docs.deprecated = docText(item.deprecation.note ?? '');
    }

    if (!item.docs?.trim()) return docs;

    const links = this.#docLinks(item);
    const parts = splitRustDocs(item.docs);

    if (parts.body) docs.summary = docText(parts.body, links);

    for (const example of parts.examples) docs.examples.push({ body: docText(example, links) });

    for (const section of parts.sections)
      docs.sections.push({ title: section.title, body: docText(section.body, links) });

    return docs;
  }

  /** Structured intra-doc links supplied by rustdoc: link text to item id. */
  #docLinks(item: RustdocItem): Record<string, DocLink> {
    this.#currentLinks = {};

    for (const [text, id] of Object.entries(item.links)) {
      const resolved = this.#referenceTarget(id);

      if (resolved.target) this.#currentLinks[text] = { target: resolved.target };
      else if (resolved.external?.url)
        this.#currentLinks[text] = { external: resolved.external.url };
    }

    return this.#currentLinks;
  }
}

function compareItems(a: RustdocItem, b: RustdocItem): number {
  return (a.name ?? '').localeCompare(b.name ?? '');
}

function abiName(abi: RustFnHeader['abi']): string {
  if (typeof abi === 'string') return abi;

  return Object.keys(abi)[0] ?? 'C';
}

/** Shorthand receiver such as `&mut self`, or undefined when `self` has an explicit type. */
function receiverText(type: RustType): string | undefined {
  if (typeof type === 'object' && 'generic' in type && type.generic === 'Self') return 'self';

  if (typeof type === 'object' && 'borrowed_ref' in type) {
    const inner = type.borrowed_ref.type;

    if (typeof inner === 'object' && 'generic' in inner && inner.generic === 'Self') {
      const lifetime = type.borrowed_ref.lifetime ? `${type.borrowed_ref.lifetime} ` : '';

      return `&${lifetime}${type.borrowed_ref.is_mutable ? 'mut ' : ''}self`;
    }
  }

  return undefined;
}

/** Attributes worth showing on a documentation page. */
function attributeText(attribute: RustdocItem['attrs'][number]): string | undefined {
  const keep = ['non_exhaustive', 'must_use', 'repr'];

  if (typeof attribute === 'string')
    return keep.some((name) => attribute === name) ? `#[${attribute}]` : undefined;

  const other = (attribute as { other?: string }).other;

  if (typeof other === 'string') {
    return keep.some((name) => other.startsWith(`#[${name}`)) ? other : undefined;
  }

  const key = Object.keys(attribute)[0];

  return key && keep.includes(key) ? `#[${key}]` : undefined;
}

export { firstParagraph };
