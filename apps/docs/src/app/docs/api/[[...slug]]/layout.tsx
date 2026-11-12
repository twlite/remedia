import { source } from '@/lib/source';
import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import { baseOptions } from '@/lib/layout.shared';
import { pruneApiTree } from '@/lib/api/tree';

/** Same sidebar as the guides, but expanded to the API package being viewed. */
export default async function Layout({ children, params }: LayoutProps<'/docs/api/[[...slug]]'>) {
  const { slug } = await params;
  const url = `/docs/api${slug?.length ? `/${slug.join('/')}` : ''}`;

  return (
    <DocsLayout tree={pruneApiTree(source.getPageTree(), url)} {...baseOptions()}>
      {children}
    </DocsLayout>
  );
}
