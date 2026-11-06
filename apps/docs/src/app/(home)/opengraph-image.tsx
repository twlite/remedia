import { createHomeSocialImage } from '@/lib/og';

export const dynamic = 'force-static';

export const alt =
  'Remedia — A modern audio engine for Discord. Rust powered, embedded and remote.';
export const size = { width: 1200, height: 630 };
export const contentType = 'image/png';

export default function Image() {
  return createHomeSocialImage();
}
