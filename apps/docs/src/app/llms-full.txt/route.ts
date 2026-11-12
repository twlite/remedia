import { guidesFullText } from '@/lib/source';

export const dynamic = 'force-static';

export async function GET() {
  return new Response(await guidesFullText());
}
