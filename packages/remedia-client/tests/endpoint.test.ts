import { describe, expect, it } from 'vitest';
import { parseRemoteEndpoint } from '../lib/endpoint.js';

describe('remote endpoint parsing', () => {
  it.each([
    ['remedia://example.test:8787', 'ws://example.test:8787/v1/ws'],
    ['remedias://example.test:8787', 'wss://example.test:8787/v1/ws'],
    ['ws://example.test:8787', 'ws://example.test:8787/v1/ws'],
    ['wss://example.test:8787/custom', 'wss://example.test:8787/custom'],
  ])('maps %s to its WebSocket endpoint', (input, expected) => {
    expect(parseRemoteEndpoint(input, { username: 'bot', password: 'secret' }).url).toBe(expected);
  });

  it('moves decoded URL credentials into Basic auth and strips userinfo', () => {
    const parsed = parseRemoteEndpoint('remedia://bot%40example:p%3Ass%3F@localhost:8787');
    expect(parsed.url).toBe('ws://localhost:8787/v1/ws');
    expect(parsed.authorization).toBe(
      `Basic ${Buffer.from('bot@example:p:ss?').toString('base64')}`,
    );
    expect(parsed.url).not.toContain('secret');
  });

  it('prefers explicit credentials and rejects unsupported schemes', () => {
    const parsed = parseRemoteEndpoint('remedia://old:password@localhost:8787', {
      username: 'new',
      password: 'new-secret',
    });
    expect(parsed.authorization).toBe(`Basic ${Buffer.from('new:new-secret').toString('base64')}`);
    expect(() =>
      parseRemoteEndpoint('https://localhost:8787', { username: 'bot', password: 'x' }),
    ).toThrow(/scheme/);
  });
});
