import assert from 'node:assert/strict';
import { describe, it } from 'node:test';

import type { TypeExpr } from '../model.ts';
import { pageUrl } from '../symbols.ts';
import { signatureToText, typeToText } from '../text.ts';
import { walkSymbol } from '../walk.ts';
import { member, pageOf, rustFixture } from './helpers.ts';

describe('Rust API documentation', async () => {
  const built = await rustFixture();

  it('documents traits with associated types, constants, required and provided methods', () => {
    const source = pageOf(built, 'fx-base', 'Source').symbol;

    assert.deepEqual(
      source.members.map((entry) => [entry.name, entry.kind, entry.rust?.provided]),
      [
        ['Item', 'associated-type', undefined],
        ['LIMIT', 'associated-constant', undefined],
        ['next_item', 'method', false],
        ['peek', 'method', true],
      ],
    );
    assert.equal(member(source, 'LIMIT').value, '8');
  });

  it('lists implementors and links them to the implementing type', () => {
    const source = pageOf(built, 'fx-base', 'Source').symbol;
    const counter = source.rust?.implementors.find(
      (entry) => typeToText(entry.forType) === 'Counter',
    );

    assert.equal(counter?.href, '/docs/api/rust/fx-base/Counter/#impl-Source-for-Counter');
  });

  it('renders trait implementations with associated items on the implementing type', () => {
    const counter = pageOf(built, 'fx-base', 'Counter').symbol;
    const impl = counter.rust?.impls.find((entry) => entry.anchor === 'impl-Source-for-Counter');

    assert.equal(impl?.implKind, 'trait');
    assert.deepEqual(
      impl?.items.map((entry) => [entry.name, entry.kind]),
      [
        ['Item', 'associated-type'],
        ['next_item', 'method'],
      ],
    );
    assert.equal(
      impl?.trait?.kind === 'reference' ? impl.trait.href : undefined,
      '/docs/api/rust/fx-base/Source/',
    );
    assert.ok(
      counter.rust?.impls.some(
        (entry) => entry.implKind === 'inherent' && entry.items[0]?.name === 'new',
      ),
    );
    assert.ok(counter.rust?.impls.some((entry) => entry.implKind === 'auto'));
    assert.ok(counter.rust?.impls.some((entry) => entry.implKind === 'blanket'));
    assert.deepEqual(counter.rust?.attributes, ['#[non_exhaustive]']);
    assert.ok(counter.rust?.hasStrippedFields, 'private fields are not documented');
    assert.deepEqual(
      counter.members.map((entry) => entry.name),
      ['value'],
    );
  });

  it('links associated types through their trait, not by name', () => {
    const drain = pageOf(built, 'fx-app', 'drain').symbol;
    const signature = drain.signatures[0];

    assert.equal(
      signatureToText(signature as NonNullable<typeof signature>),
      'fn drain<S: Source>(source: &mut S) -> Vec<<S as Source>::Item>',
    );

    let qualified: Extract<TypeExpr, { kind: 'qualified-path' }> | undefined;

    walkSymbol(drain, {
      type(type) {
        if (type.kind === 'qualified-path') qualified = type;
      },
    });

    assert.equal(qualified?.href, '/docs/api/rust/fx-base/Source/#associatedtype-Item');
  });

  it('links across crates', () => {
    const wrapper = pageOf(built, 'fx-app', 'Wrapper').symbol;
    const field = member(wrapper, 'inner').type;
    const trait = wrapper.rust?.impls.find((entry) => entry.implKind === 'trait')?.trait;

    assert.equal(
      field?.kind === 'reference' ? field.href : undefined,
      '/docs/api/rust/fx-base/Counter/',
    );
    assert.equal(
      trait?.kind === 'reference' ? trait.href : undefined,
      '/docs/api/rust/fx-base/Source/',
    );
  });

  it('documents items re-exported from private modules at the re-export path', () => {
    const hidden = pageOf(built, 'fx-base', 'Hidden');

    assert.equal(pageUrl(hidden.slugs), '/docs/api/rust/fx-base/Hidden/');
    assert.deepEqual(
      hidden.symbol.id.path,
      ['private', 'Hidden'],
      'identity keeps the defining path',
    );
  });

  it('keeps same-named items in different modules and namespaces apart', () => {
    const urls = built.table.pages
      .filter((page) => page.pkg.name === 'fx-base')
      .map((page) => pageUrl(page.slugs));

    assert.ok(urls.includes('/docs/api/rust/fx-base/config/Settings/'));
    assert.ok(urls.includes('/docs/api/rust/fx-base/other/Settings/'));
    assert.ok(urls.includes('/docs/api/rust/fx-base/Twin/'));
    assert.ok(
      urls.includes('/docs/api/rust/fx-base/Twin-fn/'),
      'a function sharing a type name gets a kind suffix',
    );
    assert.equal(
      new Set(urls.map((url) => url.toLowerCase())).size,
      urls.length,
      'URLs are unique case-insensitively',
    );
    assert.ok(urls.includes('/docs/api/rust/fx-base/COUNTER-struct/'));
    assert.ok(urls.includes('/docs/api/rust/fx-base/counter-fn/'));
  });

  it('describes enum variants by shape', () => {
    const shape = pageOf(built, 'fx-base', 'Shape').symbol;

    assert.deepEqual(
      shape.members.map((entry) => [
        entry.name,
        entry.rust?.structKind,
        entry.members.map((field) => field.name),
      ]),
      [
        ['Empty', 'unit', []],
        ['Circle', 'tuple', ['0']],
        ['Rect', 'plain', ['width', 'height']],
      ],
    );
  });

  it('keeps rustdoc sections and examples, hides doctest setup lines, and resolves intra-doc links', () => {
    const source = pageOf(built, 'fx-base', 'Source').symbol;
    const next = member(source, 'next_item');

    assert.equal(source.docs.examples.length, 1);
    assert.equal(source.docs.examples[0]?.body.markdown, '```rust\nlet item = s.next_item();\n```');
    assert.deepEqual(
      next.docs.sections.map((section) => section.title),
      ['Errors'],
    );

    const links = Object.values(next.docs.summary?.links ?? {}).map((link) => link.href);

    assert.deepEqual(links.sort(), [
      '/docs/api/rust/fx-base/Counter/',
      '/docs/api/rust/fx-base/Source/#associatedtype-Item',
    ]);
  });

  it('has no unlinked references in the fixture', () => {
    assert.deepEqual(built.unresolved, []);
  });
});
