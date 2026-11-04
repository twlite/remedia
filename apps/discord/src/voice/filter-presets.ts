/** Bot-only names mapped to raw graphs understood by the native FFmpeg filtergraph API. */
export const FILTER_PRESETS = {
  '8d': 'apulsator=mode=sine:hz=0.125:amount=0.7',
  '16d': [
    'acrossover=split=250 2500:order=4th[lo][mid][hi];' +
      '[lo]apulsator=mode=sine:hz=0.05:amount=0.3[lo2];' +
      '[mid]apulsator=mode=sine:hz=0.11:amount=0.6:offset_l=0:offset_r=0.5[mid2];' +
      '[hi]apulsator=mode=sine:hz=0.17:amount=0.6:offset_l=0.25:offset_r=0.75[hi2];' +
      '[lo2][mid2][hi2]amix=inputs=3:normalize=0',
    'crossfeed=strength=0.4',
    'lowpass=f=4000',
    'aecho=0.9:0.9:40|70:0.15|0.1',
    'bass=g=2:f=100:w=0.6',
  ].join(','),
  bassboost: 'bass=gain=15:frequency=110,acompressor',
  nightcore: 'asetrate=60000,aresample=48000',
  vaporwave: 'asetrate=38400,aresample=48000',
  tremolo: 'tremolo=f=5:d=0.6',
  vibrato: 'vibrato=f=5:d=0.5',
} as const;

export type FilterPresetName = keyof typeof FILTER_PRESETS;
export const FILTER_PRESET_NAMES = Object.keys(FILTER_PRESETS) as FilterPresetName[];
