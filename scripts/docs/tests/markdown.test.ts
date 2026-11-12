import assert from 'node:assert/strict';
import { describe, it } from 'node:test';

import { linkKeys } from '../link-keys.ts';
import { firstParagraph, inlineSummary, markdownToPlainText, scanLines } from '../docs-util.ts';
import { fenceLanguage, splitRustDocs } from '../rustdoc-markdown.ts';

describe('documentation Markdown handling', () => {
  it('tracks fenced code so headings inside fences are not sections', () => {
    const parts = splitRustDocs(
      'Intro.\n\n```text\n# Errors\nnot a heading\n```\n\n# Errors\n\nIt fails.',
    );

    assert.deepEqual(parts.sections, [{ title: 'Errors', body: 'It fails.' }]);
    assert.match(parts.body, /# Errors\nnot a heading/);
  });

  it('normalizes rustdoc fence attributes and hides doctest setup lines', () => {
    const parts = splitRustDocs(
      '# Examples\n\n```no_run\n# use a::b;\n## literal hash\nlet x = 1;\n```\n\n```ignore,rust\nfoo();\n```',
    );
    const [example] = parts.examples;

    assert.equal(parts.examples.length, 1);
    assert.equal(example, '```rust\n# literal hash\nlet x = 1;\n```\n\n```rust\nfoo();\n```');
  });

  it('keeps explicit languages and plain text fences', () => {
    assert.equal(fenceLanguage(''), 'rust');
    assert.equal(fenceLanguage('should_panic'), 'rust');
    assert.equal(fenceLanguage('toml'), 'toml');
    assert.equal(fenceLanguage('text'), 'text');
    assert.equal(fenceLanguage('compile_fail,E0277'), 'rust');
  });

  it('keeps Markdown formatting and code in descriptions', () => {
    const parts = splitRustDocs(
      'Uses **bold**, `code`, and\n\n- a list\n- of items\n\n| a | b |\n|---|---|\n| 1 | 2 |',
    );

    assert.match(parts.body, /\*\*bold\*\*/);
    assert.match(parts.body, /- a list/);
    assert.match(parts.body, /\| a \| b \|/);
  });

  it('extracts the first paragraph and plain text for summaries and search', () => {
    assert.equal(
      firstParagraph('First line\nsecond line.\n\nSecond paragraph.'),
      'First line second line.',
    );
    assert.equal(firstParagraph('```ts\ncode\n```\n\nAfter.'), 'After.');
    assert.equal(
      markdownToPlainText('A [link](x) with `code` and **bold**.'),
      'A link with code and bold.',
    );
    assert.equal(inlineSummary('See [`Foo`] and [text](url).'), 'See `Foo` and text.');
  });

  it('scanLines reports fence state per line', () => {
    const seen: string[] = [];

    scanLines('a\n```\nb\n```\nc', (line, state) => seen.push(`${line}:${state.inFence}`));

    assert.deepEqual(seen, ['a:false', '```:true', 'b:true', '```:true', 'c:false']);
  });

  it('resolves rustdoc link labels through the same normalization rustdoc uses', () => {
    assert.deepEqual(linkKeys('`Foo`'), ['`Foo`', 'Foo']);
    assert.deepEqual(linkKeys('`foo()`'), ['`foo()`', 'foo()', 'foo']);
    assert.ok(linkKeys('struct@Foo').includes('Foo'));
    assert.ok(linkKeys('`vec!`').includes('vec'));
  });
});
