import { docsLlms } from '@/lib/source';

export const dynamic = 'force-static';

export async function GET() {
  const index = await docsLlms.index();
  const basePath = process.env.NEXT_PUBLIC_BASE_PATH ?? '';
  return new Response(index.replaceAll('](/docs', `](${basePath}/docs`));
}
