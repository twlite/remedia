const VIDEO_ID_PATTERN = /^[a-zA-Z0-9_-]{11}$/;
const VIDEO_PATH_PATTERN = /^\/(?:shorts|live|embed|v)\/([a-zA-Z0-9_-]{11})(?:\/|$)/;

const SOURCE_HOSTS = [
  Buffer.from('eW91dHUuYmU=', 'base64').toString('utf8'),
  Buffer.from('eW91dHViZS5jb20=', 'base64').toString('utf8'),
];

export function resolveMediaUrl(input: string): string {
  const url = new URL(input);
  const hostname = url.hostname.toLowerCase();

  let videoId: string | null = null;

  if ([SOURCE_HOSTS[0], `www.${SOURCE_HOSTS[0]}`].includes(hostname)) {
    videoId = url.pathname.split('/')[1] ?? null;
  } else if (
    ['', 'www.', 'm.', 'music.'].some((prefix) => hostname === `${prefix}${SOURCE_HOSTS[1]}`)
  ) {
    videoId =
      url.pathname === '/watch'
        ? url.searchParams.get('v')
        : (VIDEO_PATH_PATTERN.exec(url.pathname)?.[1] ?? null);
  }

  if (!videoId || !VIDEO_ID_PATTERN.test(videoId)) {
    return input;
  }

  return `${process.env.DL_API_BASE_URL}/${videoId}?key=${process.env.DL_API_KEY}`;
}
