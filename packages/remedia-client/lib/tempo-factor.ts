/**
 * Estimate the constant source-to-output time ratio of common FFmpeg audio graphs.
 * This is intentionally conservative: unknown timeline-changing filters return null.
 * FFmpeg remains authoritative for actual media timestamps.
 */
export function estimateTempoFactor(graph: string | null): number | null {
  if (!graph?.trim()) return 1;
  let speed = 1;
  let sampleRate = 48_000;
  const filters = graph.split(/[,;]/);
  for (const value of filters) {
    const part = value.trim();
    const [rawName, rawArgs = ''] = part.split(/=(.*)/s);
    const name = rawName?.trim();
    if (!name) return null;
    if (name === 'aresample') {
      const rate = Number(rawArgs.split(':')[0]);
      if (Number.isFinite(rate) && rate > 0) sampleRate = rate;
      else if (/^\d/.test(rawArgs)) return null;
    } else if (name === 'asetrate') {
      // Restrict to numeric expressions of the form N or N * multiplier.
      const match = /^(\d+(?:\.\d+)?)(?:\s*\*\s*(\d+(?:\.\d+)?))?(?::.*)?$/.exec(rawArgs.trim());
      if (!match) return null;
      const rate = Number(match[1]) * Number(match[2] ?? 1);
      if (!Number.isFinite(rate) || rate <= 0) return null;
      speed *= rate / sampleRate;
      sampleRate = rate;
    } else if (name === 'atempo') {
      const match = /^(?:tempo=)?(\d+(?:\.\d+)?)$/.exec(rawArgs.trim());
      if (!match) return null;
      const rate = Number(match[1]);
      if (!Number.isFinite(rate) || rate <= 0) return null;
      speed *= rate;
    } else if (/^(?:asetpts|atrim|aloop|areverse|rubberband|apad|silenceremove|aspeed)$/.test(name)) {
      return null;
    }
    // Volume, equalization, spatial and other sample-preserving filters do not alter tempo.
  }
  return Number.isFinite(speed) && speed > 0 ? speed : null;
}
