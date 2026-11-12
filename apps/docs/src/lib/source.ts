import { llms, loader, type InferPageType } from 'fumadocs-core/source';
import { lucideIconsPlugin } from 'fumadocs-core/source/lucide-icons';
import { docsRoute } from './shared';
import { defineDocs } from 'fumadocs-mdx/macro';
import { metaSchema, pageSchema } from 'fumadocs-core/source/schema';
import { createApiFiles } from './api/files';
import { apiPageToMarkdown } from './api/markdown';
import { getManifestPage, isApiPath, loadApiPage } from './api/store';

const docs = defineDocs({
  dir: 'content/docs',
  docs: {
    schema: pageSchema,
    postprocess: {
      includeProcessedMarkdown: true,
    },
  },
  meta: {
    schema: metaSchema,
  },
});

const guides = docs.toFumadocsSource();

// Generated API pages are virtual files merged beside the authored MDX guides, so navigation,
// breadcrumbs, search and the page tree treat both the same way. Their content is loaded by
// `src/lib/api/store.ts`; see https://fumadocs.dev/docs/headless/source-api.
export const source = loader({
  baseUrl: docsRoute,
  source: {
    files: [...guides.files, ...(createApiFiles() as unknown as typeof guides.files)],
  },
  plugins: [lucideIconsPlugin()],
});

/** Markdown body of any page: processed MDX for guides, generated Markdown for API pages. */
async function pageMarkdown(page: InferPageType<typeof source>): Promise<string> {
  if (isApiPath(page.path)) {
    const manifestPage = getManifestPage(page.url);

    return manifestPage ? apiPageToMarkdown(loadApiPage(manifestPage)) : '';
  }

  return (page.data as unknown as { getText(type: 'processed'): Promise<string> }).getText(
    'processed',
  );
}

export const docsLlms = llms(source, {
  renderPage: async (
    page,
  ) => `# ${page.data.title} (${process.env.NEXT_PUBLIC_BASE_PATH ?? ''}${page.url})

${await pageMarkdown(page)}`,
});

/** Authored guides only. The generated API reference is large and has its own per-page Markdown files. */
export async function guidesFullText(): Promise<string> {
  const pages = source.getPages().filter((page) => !isApiPath(page.path));

  return (await Promise.all(pages.map((page) => docsLlms.page(page)))).join('\n\n');
}
