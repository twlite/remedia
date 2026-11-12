import { source } from '@/lib/source';
import {
  DocsBody,
  DocsPage,
  DocsTitle,
  MarkdownCopyButton,
  ViewOptionsPopover,
} from 'fumadocs-ui/layouts/docs/page';
import { notFound } from 'next/navigation';
import type { Metadata } from 'next';
import { getPageImageUrl, getPageMarkdownUrl, gitConfig } from '@/lib/shared';
import { getManifestPage, isApiPath, loadApiPage } from '@/lib/api/store';
import { ApiPageBody } from '@/components/api/api-page';

/** Look up a generated API page from the optional catch-all segments below `/docs/api`. */
function findApiPage(slug: string[] | undefined) {
  const page = source.getPage(['api', ...(slug ?? [])]);

  return page && isApiPath(page.path) ? page : undefined;
}

export default async function Page(props: PageProps<'/docs/api/[[...slug]]'>) {
  const params = await props.params;
  const page = findApiPage(params.slug);
  const manifestPage = page ? getManifestPage(page.url) : undefined;

  if (!page || !manifestPage) notFound();

  const file = loadApiPage(manifestPage);
  const markdownUrl = getPageMarkdownUrl(page).url;

  return (
    <DocsPage toc={page.data.toc}>
      <DocsTitle>{page.data.title}</DocsTitle>
      <div className="flex flex-row gap-2 items-center border-b pb-6">
        <MarkdownCopyButton markdownUrl={markdownUrl} />
        <ViewOptionsPopover
          markdownUrl={markdownUrl}
          githubUrl={
            file.type === 'symbol' && file.page.sourceUrl
              ? file.page.sourceUrl
              : `https://github.com/${gitConfig.user}/${gitConfig.repo}`
          }
        />
      </div>
      <DocsBody>
        <ApiPageBody file={file} />
      </DocsBody>
    </DocsPage>
  );
}

export async function generateStaticParams() {
  return source
    .generateParams()
    .filter((params) => params.slug?.[0] === 'api')
    .map((params) => ({ slug: (params.slug ?? []).slice(1) }));
}

export async function generateMetadata(
  props: PageProps<'/docs/api/[[...slug]]'>,
): Promise<Metadata> {
  const params = await props.params;
  const page = findApiPage(params.slug);

  if (!page) notFound();

  const image = getPageImageUrl(page).url;

  return {
    title: page.data.title,
    description: page.data.description,
    twitter: {
      card: 'summary_large_image',
      title: page.data.title,
      description: page.data.description,
      images: image,
    },
    openGraph: {
      title: page.data.title,
      description: page.data.description,
      images: [
        {
          url: image,
          width: 1200,
          height: 630,
          alt: `${page.data.title} — Remedia API reference`,
        },
      ],
    },
  };
}
