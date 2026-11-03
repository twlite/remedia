/** Bot-only names mapped to raw graphs understood by the native FFmpeg filtergraph API. */
export const FILTER_PRESETS = {
  '8d': 'apulsator=mode=sine:hz=0.125:amount=0.7',
  bassboost: 'bass=gain=15:frequency=110,acompressor',
  nightcore: 'asetrate=60000,aresample=48000',
  vaporwave: 'asetrate=38400,aresample=48000',
  tremolo: 'tremolo=f=5:d=0.6',
  vibrato: 'vibrato=f=5:d=0.5',
} as const;

export type FilterPresetName = keyof typeof FILTER_PRESETS;
export const FILTER_PRESET_NAMES = Object.keys(FILTER_PRESETS) as FilterPresetName[];
