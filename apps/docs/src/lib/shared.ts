import { createGetUrl } from 'fumadocs-core/source';

/** Invitation to the AndrozDev support community shared with Discord Player. */
export const discordInviteUrl = 'https://androz2091.fr/discord';

export const appName = 'Remedia';
export const docsRoute = '/docs';
export const docsImageRoute = '/og/docs';
export const docsContentRoute = '/llms.mdx/docs';

export const gitConfig = {
  user: 'twlite',
  repo: 'remedia',
  branch: 'main',
};

const getContentUrl = createGetUrl(docsContentRoute);

export function getPageMarkdownUrl(page: { slugs: string[]; locale?: string | undefined }) {
  const segments = [...page.slugs, 'content.md'];

  return {
    segments,
    url: `${process.env.NEXT_PUBLIC_BASE_PATH ?? ''}${getContentUrl(segments, page.locale)}`,
  };
}

const getImageUrl = createGetUrl(docsImageRoute);

/**
 * Slugs of the page whose social image represents a page. Generated API pages share the image of
 * their package (`api/<language>/<package>`) instead of rendering one PNG per symbol.
 */
export function getImageSlugs(slugs: string[]): string[] {
  return slugs[0] === 'api' ? slugs.slice(0, 3) : slugs;
}

export function getPageImageUrl(page: { slugs: string[]; locale?: string | undefined }) {
  const segments = [...getImageSlugs(page.slugs), 'image.png'];

  return {
    segments,
    url: getImageUrl(segments, page.locale),
  };
}
