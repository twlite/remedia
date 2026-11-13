import { describe, expect, it } from 'vitest';
import { estimateTempoFactor } from '../lib/tempo-factor.js';

describe('constant FFmpeg tempo estimation', () => {
  it('handles the nightcore and vaporwave presets', () => {
    expect(estimateTempoFactor('aresample=48000,asetrate=48000*1.25')).toBe(1.25);
    expect(estimateTempoFactor('aresample=48000,asetrate=48000*0.8')).toBe(0.8);
  });

  it('combines atempo and resampled asetrate filters', () => {
    expect(estimateTempoFactor('atempo=1.5,atempo=0.8')).toBeCloseTo(1.2);
    expect(estimateTempoFactor('aresample=44100,asetrate=44100*1.25,atempo=0.8')).toBe(1);
    expect(estimateTempoFactor('bass=g=10,aresample=48000,asetrate=48000*1.25')).toBe(1.25);
  });

  it('rejects unknown rate expressions and filters that alter the media timeline', () => {
    expect(estimateTempoFactor('asetrate=sample_rate*1.25')).toBeNull();
    expect(estimateTempoFactor('atempo=foo')).toBeNull();
    expect(estimateTempoFactor('areverse')).toBeNull();
    expect(estimateTempoFactor(null)).toBe(1);
  });
});
