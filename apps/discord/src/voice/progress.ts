/** Format playback time as mm:ss, switching to hh:mm:ss for long media. */
export function formatTimecode(milliseconds: number | null): string {
  if (milliseconds === null || !Number.isFinite(milliseconds) || milliseconds < 0) return 'unknown';
  const seconds = Math.floor(milliseconds / 1_000);
  const hours = Math.floor(seconds / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);
  const remainder = seconds % 60;
  return hours > 0
    ? `${hours.toString().padStart(2, '0')}:${minutes.toString().padStart(2, '0')}:${remainder
        .toString()
        .padStart(2, '0')}`
    : `${Math.floor(seconds / 60)
        .toString()
        .padStart(2, '0')}:${remainder.toString().padStart(2, '0')}`;
}

/** Build a compact progress bar, or return null when duration is not measurable. */
export function createProgressBar(
  positionMs: number,
  durationMs: number | null,
  width = 18,
): string | null {
  if (durationMs === null || !Number.isFinite(durationMs) || durationMs <= 0) return null;
  const barWidth = Number.isFinite(width) ? Math.max(3, Math.floor(width)) : 18;
  const position = Number.isFinite(positionMs) ? Math.max(0, positionMs) : 0;
  const ratio = Math.min(1, position / durationMs);
  const indicator = Math.min(barWidth - 1, Math.floor(ratio * (barWidth - 1)));
  return Array.from({ length: barWidth }, (_, index) => {
    if (index === indicator) return '●';
    return index < indicator ? '▬' : '─';
  }).join('');
}

/** Format elapsed, progress, and total duration without inventing a percentage for live media. */
export function formatProgressLine(
  positionMs: number,
  durationMs: number | null,
  options: { estimated?: boolean; width?: number } = {},
): string {
  const elapsed = Number.isFinite(positionMs) ? Math.max(0, positionMs) : 0;
  const prefix = options.estimated ? '~' : '';
  if (durationMs === null || !Number.isFinite(durationMs) || durationMs <= 0) {
    return `${prefix}${formatTimecode(elapsed)} ┃ Duration unknown`;
  }
  const boundedDuration = Math.max(0, durationMs);
  const boundedElapsed = Math.min(elapsed, boundedDuration);
  const bar = createProgressBar(boundedElapsed, boundedDuration, options.width);
  return `${prefix}${formatTimecode(boundedElapsed)} ┃ ${bar} ┃ ${formatTimecode(boundedDuration)}`;
}
