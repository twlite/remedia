import { scanLines } from './docs-util.ts';

/** Rustdoc code-fence attributes that do not name a language. */
const FENCE_ATTRIBUTES = new Set([
  'rust',
  'no_run',
  'ignore',
  'should_panic',
  'compile_fail',
  'test_harness',
  'standalone_crate',
  'edition2015',
  'edition2018',
  'edition2021',
  'edition2024',
]);

/** Level-one headings rustdoc conventions treat as sections. */
const SECTION_TITLES = new Map<string, string>([
  ['errors', 'Errors'],
  ['panics', 'Panics'],
  ['safety', 'Safety'],
  ['aborts', 'Aborts'],
  ['undefined behavior', 'Undefined behavior'],
  ['platform-specific behavior', 'Platform-specific behavior'],
]);

/** Parsed rustdoc comment. */
export interface RustDocParts {
  body: string;
  examples: string[];
  sections: { title: string; body: string }[];
}

/**
 * Normalize a rustdoc code fence info string.
 *
 * @param info Text after the opening fence marker.
 * @returns A Markdown language tag, `rust` when only rustdoc attributes are present, or `text`.
 */
export function fenceLanguage(info: string): string {
  const tokens = info
    .split(/[\s,]+/)
    .map((token) => token.trim())
    .filter(Boolean);

  if (tokens.length === 0) return 'rust';

  if (tokens.includes('text') || tokens.includes('plain')) return 'text';

  const language = tokens.find((token) => !FENCE_ATTRIBUTES.has(token) && !/^E\d{4}$/.test(token));

  return language ?? 'rust';
}

/**
 * Split rustdoc Markdown into description, examples, and known sections, rewriting fences.
 *
 * Hidden doctest lines (`# use ...`) are removed because rustdoc does not display them, and fence
 * info strings such as `no_run` or `ignore` become plain `rust` fences.
 *
 * @param markdown Raw doc comment.
 * @returns Parsed parts; each part is Markdown.
 */
export function splitRustDocs(markdown: string): RustDocParts {
  const body: string[] = [];
  const examples: string[][] = [];
  const sections: { title: string; lines: string[] }[] = [];
  let target: string[] = body;
  let fenceLang: string | undefined;
  let fenceMarker = '';

  scanLines(markdown, (line, state) => {
    if (state.fenceOpen) {
      const trimmed = line.trimStart();
      const marker = trimmed.match(/^(`{3,}|~{3,})/)?.[1] ?? '```';

      fenceMarker = marker;
      fenceLang = fenceLanguage(trimmed.slice(marker.length));
      target.push(`${line.slice(0, line.length - trimmed.length)}${marker}${fenceLang}`);

      return;
    }

    if (state.fenceClose) {
      target.push(line);
      fenceLang = undefined;
      fenceMarker = '';

      return;
    }

    if (state.inFence) {
      if (fenceLang === 'rust') {
        const hidden = line.trim();

        if (hidden === '#') return;

        if (hidden.startsWith('## ')) {
          target.push(line.replace('## ', '# '));

          return;
        }

        if (hidden.startsWith('# ')) return;
      }

      target.push(line);

      return;
    }

    const heading = line.match(/^#\s+(.+?)\s*#*$/);

    if (heading) {
      const title = (heading[1] ?? '').trim();
      const lower = title.toLowerCase();

      if (lower === 'examples' || lower === 'example') {
        const lines: string[] = [];

        examples.push(lines);
        target = lines;

        return;
      }

      const known = SECTION_TITLES.get(lower);

      if (known) {
        const section = { title: known, lines: [] as string[] };

        sections.push(section);
        target = section.lines;

        return;
      }
    }

    target.push(line);
  });

  void fenceMarker;

  return {
    body: body.join('\n').trim(),
    examples: examples.map((lines) => lines.join('\n').trim()).filter(Boolean),
    sections: sections
      .map((section) => ({ title: section.title, body: section.lines.join('\n').trim() }))
      .filter((section) => section.body),
  };
}
