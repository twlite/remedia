import assert from 'node:assert/strict';
import { PassThrough } from 'node:stream';
import { test } from 'vitest';

import type { NativeRemedia, NativeVoiceOptions } from '../native.js';
import { DiscordSession, type DiscordSessionOptions } from '../lib/discord.js';
import { DiscordGatewayAdapter, type DiscordGatewayVoicePayload } from '../lib/gateway.js';

const sessionOptions = {
  guildId: '123456789012345678',
  channelId: '234567890123456789',
  userId: '345678901234567890',
};

test('sends OP 4 and waits for matching voice state and server updates in either order', async () => {
  const harness = createHarness();
  const session = harness.session();

  // Dispatches received before this session requested a join are stale and must not be cached.
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('stale-session'), 1));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('stale-token'), 2));

  const connected = session.connect({ timeoutMs: 1_000 });
  assert.deepEqual(
    harness.sent.map(({ payload }) => payload),
    [op4(sessionOptions.guildId, sessionOptions.channelId)],
  );

  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('fresh-token'), 3));
  harness.receive(
    gatewayPacket(
      'VOICE_STATE_UPDATE',
      { ...voiceState('unrelated-user-session'), user_id: '456789012345678901' },
      4,
    ),
  );
  await flushMicrotasks();
  assert.equal(harness.connections.length, 0);

  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('fresh-session'), 5));
  await connected;

  assert.equal(session.connectionState, 'connected');
  assert.deepEqual(harness.connections, [
    {
      guildId: sessionOptions.guildId,
      channelId: sessionOptions.channelId,
      userId: sessionOptions.userId,
      sessionId: 'fresh-session',
      token: 'fresh-token',
      endpoint: 'voice.example.test:443',
    },
  ]);
});

test('ignores unrelated Gateway packets and does not send OP 4 during voice resume', async () => {
  const harness = createHarness();
  const session = harness.session();
  const connected = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 2));
  await connected;

  const dispatch = harness.gateway.receive({ op: 9, s: 3, d: {} });
  harness.receive({ op: 0, t: 'RESUMED', s: 4, d: {} });
  harness.setVoiceState('voice-1', 'reconnecting');
  await session.connect();
  assert.equal(dispatch, undefined);
  assert.equal(harness.sent.length, 1);
  assert.equal(harness.connections.length, 1);
});

test('accepts fresh dispatch sequences after the host Gateway starts a new READY epoch', async () => {
  const harness = createHarness();
  const session = harness.session();
  harness.receive(gatewayPacket('READY', {}, 900));
  const connected = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('old-gateway-session'), 901));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('old-gateway-token'), 902));
  await connected;

  harness.receive(gatewayPacket('READY', {}, 1));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('new-gateway-token'), 2));
  const migrated = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('new-gateway-session'), 3));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('new-gateway-token'), 4));
  await migrated;
  await waitFor(() => (harness.replacements.length === 1 ? true : undefined));

  assert.equal(session.connectionState, 'connected');
  assert.equal(harness.replacements[0]?.options.sessionId, 'new-gateway-session');
  await session.disconnect();
});

test('correlates concurrent joins by guild and keeps their credentials independent', async () => {
  const harness = createHarness();
  const first = harness.session();
  const secondOptions = {
    guildId: '456789012345678901',
    channelId: '567890123456789012',
    userId: '678901234567890123',
  };
  const second = harness.session(secondOptions);
  const firstConnected = first.connect();
  const secondConnected = second.connect();

  harness.receive(
    gatewayPacket(
      'VOICE_SERVER_UPDATE',
      voiceServer('second-token', undefined, secondOptions.guildId),
      1,
    ),
  );
  harness.receive(
    gatewayPacket('VOICE_STATE_UPDATE', voiceState('second-session', secondOptions), 2),
  );
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('first-session'), 3));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('first-token'), 4));
  await Promise.all([firstConnected, secondConnected]);

  assert.equal(
    harness.connections.find((connection) => connection.guildId === sessionOptions.guildId)?.token,
    'first-token',
  );
  assert.equal(
    harness.connections.find((connection) => connection.guildId === secondOptions.guildId)?.token,
    'second-token',
  );
  await Promise.all([first.disconnect(), second.disconnect()]);
});

test('voice server changes replace only the transport and preserve the playback session', async () => {
  const harness = createHarness();
  const session = harness.session();
  const connected = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 2));
  await connected;

  harness.receive(
    gatewayPacket(
      'VOICE_SERVER_UPDATE',
      voiceServer('rotated-token', 'new-voice.example.test:443'),
      3,
    ),
  );
  const reconnected = session.connect();
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('stale-token'), 2));
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('new-session'), 4));
  await flushMicrotasks();
  assert.equal(harness.connections.length, 1);
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('new-token'), 5));
  await reconnected;
  await waitFor(() => (harness.replacements.length === 1 ? true : undefined));
  await waitFor(() => (session.connectionState === 'connected' ? true : undefined));

  assert.equal(harness.replacements[0]?.options.token, 'new-token');
  assert.equal(harness.replacements[0]?.options.endpoint, 'voice.example.test:443');
  assert.equal(harness.replacements[0]?.options.sessionId, 'new-session');
  assert.equal(harness.replacements[0]?.voiceId, 'voice-1');
  assert.deepEqual(
    harness.sent.map(({ payload }) => payload),
    [
      op4(sessionOptions.guildId, sessionOptions.channelId),
      op4(sessionOptions.guildId, sessionOptions.channelId),
    ],
  );
  assert.deepEqual(harness.disconnectedVoiceIds, []);
  assert.deepEqual(harness.releasedVoiceIds, []);
  await session.disconnect();
});

test('region migration while paused keeps the same playback id and repeated credentials are idempotent', async () => {
  const harness = createHarness();
  const session = harness.session();
  const connected = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 2));
  await connected;
  let startedEvents = 0;
  session.on('playbackStarted', () => (startedEvents += 1));
  await session.play({ url: 'https://media.example/track.wav' });
  await session.pause();
  const playbackId = session.generation;

  harness.receive(
    gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated', 'region-2.example.test:443'), 3),
  );
  const recovery = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session-2'), 4));
  harness.receive(
    gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated', 'region-2.example.test:443'), 5),
  );
  await recovery;
  await waitFor(() => (harness.replacements.length === 1 ? true : undefined));

  assert.equal(session.state, 'paused');
  assert.equal(session.generation, playbackId);
  assert.equal(startedEvents, 1);
  harness.receive(
    gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated', 'region-2.example.test:443'), 6),
  );
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('stale-token'), 4));
  await flushMicrotasks();
  assert.equal(harness.sent.length, 2);
  assert.equal(harness.replacements.length, 1);
  await session.disconnect();
});

test('a server update alone uses the latest token when the endpoint is unchanged', async () => {
  const harness = createHarness();
  const session = harness.session();
  const connected = session.connect();
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('initial-token'), 1));
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 2));
  await connected;
  await session.play({ url: 'https://media.example/token-rotation.wav' });
  const playbackGeneration = session.generation;

  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('latest-token'), 3));
  const recovery = session.connect();
  await recovery;
  await waitFor(() => (harness.replacements.length === 1 ? true : undefined));

  assert.equal(harness.replacements[0]?.options.token, 'latest-token');
  assert.equal(harness.replacements[0]?.options.endpoint, 'voice.example.test:443');
  assert.equal(harness.replacements[0]?.options.sessionId, 'session');
  assert.equal(harness.replacements[0]?.voiceId, 'voice-1');
  assert.equal(session.generation, playbackGeneration);
  assert.equal(session.state, 'playing');
  assert.equal(harness.sent.length, 2);
  await session.disconnect();
});

test('a temporary null endpoint parks recovery until fresh credentials arrive', async () => {
  const harness = createHarness();
  const session = harness.session();
  const connected = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('initial-token'), 2));
  await connected;
  await session.play({ url: 'https://media.example/temporary-null-endpoint.wav' });

  harness.receive(
    gatewayPacket('VOICE_SERVER_UPDATE', { guild_id: sessionOptions.guildId, endpoint: null }, 3),
  );
  await waitFor(() => (harness.sent.length === 2 ? true : undefined));
  await flushMicrotasks();
  assert.equal(harness.replacements.length, 0);

  // The endpoint becomes available again with the same still-valid credentials.
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('initial-token'), 4));
  await session.connect();
  await waitFor(() => (harness.replacements.length === 1 ? true : undefined));
  assert.equal(harness.replacements[0]?.options.token, 'initial-token');
  assert.equal(harness.replacements[0]?.options.sessionId, 'session');
  assert.equal(session.state, 'playing');
  await session.disconnect();
});

test('an unexpected native transport closure requests fresh credentials and keeps playback alive', async () => {
  const harness = createHarness();
  const session = harness.session();
  const connected = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('initial-token'), 2));
  await connected;
  await session.play({ url: 'https://media.example/unexpected-close.wav' });
  const playbackGeneration = session.generation;

  harness.setVoiceState('voice-1', 'disconnected');
  await waitFor(() => (harness.sent.length === 2 ? true : undefined));
  const recovery = session.connect();
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('resumed-token'), 3));
  await recovery;
  await waitFor(() => (harness.replacements.length === 1 ? true : undefined));

  assert.equal(harness.replacements[0]?.options.sessionId, 'session');
  assert.equal(harness.replacements[0]?.options.token, 'resumed-token');
  assert.equal(session.generation, playbackGeneration);
  assert.equal(session.state, 'playing');
  await session.disconnect();
});

test('a newer Gateway update invalidates an in-flight native transport replacement', async () => {
  let releaseFirstReplacement!: () => void;
  let firstReplacementStarted!: () => void;
  const firstStarted = new Promise<void>((resolve) => {
    firstReplacementStarted = resolve;
  });
  const blockedReplacement = new Promise<void>((resolve) => {
    releaseFirstReplacement = resolve;
  });
  let replacementCount = 0;
  const harness = createHarness(undefined, undefined, async () => {
    replacementCount += 1;
    if (replacementCount === 1) {
      firstReplacementStarted();
      await blockedReplacement;
    }
  });
  const session = harness.session();
  const connected = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 2));
  await connected;
  await session.play({ url: 'https://example.test/transport-race.wav' });

  harness.receive(
    gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated-1', 'region-one.test:443'), 3),
  );
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session-1'), 4));
  harness.receive(
    gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated-1', 'region-one.test:443'), 5),
  );
  await firstStarted;

  harness.receive(
    gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated-2', 'region-two.test:443'), 6),
  );
  releaseFirstReplacement();
  await waitFor(() => (harness.replacements.length === 2 ? true : undefined));
  await waitFor(() => (session.connectionState === 'connected' ? true : undefined));

  assert.equal(harness.connections.length, 1);
  assert.deepEqual(
    harness.replacements.map(({ options }) => options.endpoint),
    ['region-one.test:443', 'region-two.test:443'],
  );
  assert.equal(harness.replacements[0]?.voiceId, harness.replacements[1]?.voiceId);
  assert.equal(harness.suspendedVoiceIds.length, 1);
  assert.equal(session.state, 'playing');
  await session.disconnect();
});

test('playback controls remain available during migration and stop is never undone by recovery', async () => {
  const harness = createHarness();
  const session = harness.session();
  const connected = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 2));
  await connected;
  let startedEvents = 0;
  session.on('playbackStarted', () => (startedEvents += 1));
  await session.play({ url: 'https://media.example/track.wav' });

  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated'), 3));
  const recovery = session.connect();
  assert.equal(session.connectionState, 'reconnecting');
  await session.pause();
  assert.equal(session.state, 'paused');
  await session.stop();
  assert.equal(session.state, 'stopped');

  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session-2'), 4));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated'), 5));
  await recovery;
  await waitFor(() => (harness.replacements.length === 1 ? true : undefined));

  assert.equal(session.connectionState, 'connected');
  assert.equal(session.state, 'stopped');
  assert.equal(startedEvents, 1);
  await session.disconnect();
});

test('explicit disconnect cancels pending transport recovery', async () => {
  const harness = createHarness();
  const session = harness.session();
  const connected = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 2));
  await connected;
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated'), 3));
  const recovery = session.connect();
  const recoveryRejected = assert.rejects(recovery, /cancelled by disconnect/i);

  await session.disconnect();
  await recoveryRejected;
  assert.equal(harness.replacements.length, 0);
  assert.deepEqual(harness.sent.at(-1)?.payload, op4(sessionOptions.guildId, null));
  assert.deepEqual(harness.disconnectedVoiceIds, ['voice-1']);
});

test('failed migration retries a bounded number of times before failing playback once', async () => {
  let replacements = 0;
  const harness = createHarness(undefined, undefined, async () => {
    replacements += 1;
    throw new Error('voice transport did not become ready');
  });
  const session = harness.session();
  const connected = session.connect();
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 2));
  await connected;

  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated-1'), 3));
  const recovery = session.connect();
  const rejected = assert.rejects(recovery, /voice transport did not become ready/i);
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session-1'), 4));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated-1'), 5));
  await waitFor(() => (harness.sent.length >= 3 ? true : undefined));
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session-2'), 6));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated-2'), 7));
  await waitFor(() => (harness.sent.length >= 4 ? true : undefined));
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session-3'), 8));
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('rotated-3'), 9));
  await rejected;

  assert.equal(replacements, 3);
  assert.equal(session.connectionState, 'failed');
  assert.deepEqual(harness.disconnectedVoiceIds, []);
  assert.deepEqual(harness.releasedVoiceIds, []);
  assert.equal(harness.failedRecoveries.length, 1);
});

test('disconnect cancels a pending join and sends OP 4 with a null channel', async () => {
  const harness = createHarness();
  const session = harness.session();
  const connecting = session.connect();
  const rejected = assert.rejects(connecting, /cancelled by disconnect/i);

  await session.disconnect();
  await rejected;

  assert.deepEqual(
    harness.sent.map(({ payload }) => payload),
    [op4(sessionOptions.guildId, sessionOptions.channelId), op4(sessionOptions.guildId, null)],
  );
  assert.equal(session.connectionState, 'disconnected');
  assert.equal(harness.connections.length, 0);
});

test('connection timeout leaves the requested channel and rejects connect()', async () => {
  const harness = createHarness();
  const session = harness.session();

  await assert.rejects(session.connect({ timeoutMs: 20 }), /timed out/i);

  assert.deepEqual(
    harness.sent.map(({ payload }) => payload),
    [op4(sessionOptions.guildId, sessionOptions.channelId), op4(sessionOptions.guildId, null)],
  );
  assert.equal(session.connectionState, 'failed');
  assert.equal(harness.connections.length, 0);
});

test('AbortSignal cancels Gateway negotiation and stale dispatches cannot satisfy a later connect', async () => {
  const harness = createHarness();
  const session = harness.session();
  const abort = new AbortController();
  const connecting = session.connect({ signal: abort.signal, timeoutMs: 1_000 });
  const rejected = assert.rejects(connecting, /cancelled/i);
  abort.abort();
  await rejected;

  // A delayed server dispatch received while no join is pending is not retained for the next join.
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('old-token'), 1));
  const next = session.connect({ timeoutMs: 1_000 });
  harness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('new-session'), 2));
  await flushMicrotasks();
  assert.equal(harness.connections.length, 0);
  harness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('new-token'), 3));
  await flushMicrotasks();
  assert.equal(harness.connections.length, 1);
  await next;
  assert.equal(harness.connections[0]?.token, 'new-token');
  assert.equal(harness.connections[0]?.sessionId, 'new-session');
});

test('rejects invalid snowflakes and duplicate live guild sessions', async () => {
  const harness = createHarness();
  assert.throws(
    () => harness.session({ ...sessionOptions, userId: '18446744073709551616' }),
    /userId.*snowflake/i,
  );
  harness.session();
  assert.throws(() => harness.session(), /already exists/i);
});

test('propagates Gateway send and native transport failures', async () => {
  const sendFailure = new Error('shard send failed');
  const failedSendHarness = createHarness(() => {
    throw sendFailure;
  });
  const failedSendSession = failedSendHarness.session();
  await assert.rejects(failedSendSession.connect(), /shard send failed/);
  assert.equal(failedSendSession.connectionState, 'failed');

  const nativeFailureHarness = createHarness(undefined, async () => {
    throw new Error('voice handshake failed');
  });
  const nativeFailureSession = nativeFailureHarness.session();
  const connecting = nativeFailureSession.connect();
  nativeFailureHarness.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 1));
  nativeFailureHarness.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 2));
  await assert.rejects(connecting, /voice handshake failed/);
  assert.equal(nativeFailureSession.connectionState, 'failed');
});

function createHarness(
  send?: (guildId: string, payload: DiscordGatewayVoicePayload) => void | Promise<void>,
  connectVoice?: (options: NativeVoiceOptions) => Promise<{ voiceId: string; playbackId: string }>,
  replaceVoice?: (
    voiceId: string,
    generation: number,
    options: NativeVoiceOptions,
  ) => Promise<void>,
) {
  const sent: Array<{ guildId: string; payload: DiscordGatewayVoicePayload }> = [];
  const connections: NativeVoiceOptions[] = [];
  const replacements: Array<{ voiceId: string; options: NativeVoiceOptions }> = [];
  const disconnectedVoiceIds: string[] = [];
  const releasedVoiceIds: string[] = [];
  const failedRecoveries: string[] = [];
  const suspendedVoiceIds: string[] = [];
  const transportGenerations = new Map<string, number>();
  const voiceStates = new Map<string, string>();
  const playbackStates = new Map<string, string>();
  let nextVoiceId = 1;
  const operations: string[] = [];
  const volumes = new Map<string, number>();
  const graphs = new Map<string, string>();

  const native = {
    connectVoice: async (options: NativeVoiceOptions) => {
      connections.push(options);
      if (connectVoice) return connectVoice(options);
      const voiceId = `voice-${nextVoiceId++}`;
      voiceStates.set(voiceId, 'connected');
      const playbackId = `playback-${voiceId}`;
      playbackStates.set(playbackId, 'idle');
      return { voiceId, playbackId };
    },
    replaceVoice: async (voiceId: string, generation: number, options: NativeVoiceOptions) => {
      replacements.push({ voiceId, options });
      if (replaceVoice) await replaceVoice(voiceId, generation, options);
      if (transportGenerations.get(voiceId) !== generation)
        throw new Error('Discord voice recovery attempt was superseded');
      voiceStates.set(voiceId, 'connected');
    },
    invalidateVoiceRecovery: (voiceId: string, generation: number) => {
      transportGenerations.set(voiceId, generation);
    },
    suspendVoice: async (voiceId: string, generation: number) => {
      suspendedVoiceIds.push(voiceId);
      if (transportGenerations.get(voiceId) === generation)
        voiceStates.set(voiceId, 'disconnected');
    },
    failVoiceRecovery: (voiceId: string) => failedRecoveries.push(voiceId),
    playbackState: (playbackId: string) => playbackStates.get(playbackId) ?? 'idle',
    playbackGeneration: () => 1,
    playbackSourceGeneration: () => 1,
    validateFilterGraph: async () => {},
    playbackSetFilterGraph: async (id: string, graph: string) => {
      operations.push('filter');
      graphs.set(id, graph);
    },
    playbackSetVolume: async (id: string, gain: number) => {
      operations.push('volume');
      volumes.set(id, gain);
    },
    playbackFilterGraph: (id: string) => graphs.get(id) ?? null,
    playbackVolume: (id: string) => volumes.get(id) ?? 1,
    createByteStream: () => {
      operations.push('input');
      return 'one-shot';
    },
    writeByteStream: async () => {},
    finishByteStream: async () => {},
    cancelByteStream: () => {},
    playbackPosition: () => ({ submittedMs: 0 }),
    playbackDurationMs: () => 100_000,
    playbackError: () => null,
    play: async (playbackId: string) => {
      operations.push('play');
      playbackStates.set(playbackId, 'playing');
      return {
        streamIndex: 0,
        codec: 'wav',
        sampleRate: 48_000,
        channels: 2,
        channelLayout: 'stereo',
        durationMs: 100_000,
        metadata: {},
      };
    },
    pause: async (playbackId: string) => playbackStates.set(playbackId, 'paused'),
    resume: async (playbackId: string) => playbackStates.set(playbackId, 'playing'),
    stop: async (playbackId: string) => playbackStates.set(playbackId, 'stopped'),
    wait: async () => new Promise<string>(() => undefined),
    voiceState: (voiceId: string) => voiceStates.get(voiceId) ?? 'disconnected',
    voiceError: () => null,
    disconnectVoice: async (voiceId: string) => {
      disconnectedVoiceIds.push(voiceId);
      voiceStates.set(voiceId, 'disconnected');
    },
    releaseVoiceSession: (voiceId: string) => releasedVoiceIds.push(voiceId),
  } as unknown as NativeRemedia;
  const gateway = new DiscordGatewayAdapter((guildId, payload) => {
    sent.push({ guildId, payload });
    return send?.(guildId, payload);
  });
  const sessions = new Map<string, DiscordSession>();

  return {
    gateway,
    operations,
    sent,
    connections,
    replacements,
    disconnectedVoiceIds,
    releasedVoiceIds,
    failedRecoveries,
    suspendedVoiceIds,
    setVoiceState(voiceId: string, state: string) {
      voiceStates.set(voiceId, state);
    },
    receive(packet: unknown) {
      const dispatch = gateway.receive(packet);
      if (dispatch) {
        sessions
          .get(dispatch.guildId)
          ?.receiveGatewayDispatch(
            dispatch.event,
            dispatch.data,
            dispatch.sequence,
            dispatch.epoch,
          );
      }
    },
    session(options: DiscordSessionOptions = sessionOptions) {
      if (sessions.has(options.guildId)) {
        throw new Error(`a Discord voice session already exists for guild ${options.guildId}`);
      }
      const session = new DiscordSession(native, gateway, options, () => {
        sessions.delete(options.guildId);
      });
      sessions.set(options.guildId, session);
      return session;
    },
  };
}

function voiceState(
  sessionId: string,
  options: typeof sessionOptions = sessionOptions,
): Record<string, unknown> {
  return {
    guild_id: options.guildId,
    user_id: options.userId,
    channel_id: options.channelId,
    session_id: sessionId,
  };
}

function voiceServer(
  token: string,
  endpoint = 'voice.example.test:443',
  guildId = sessionOptions.guildId,
): Record<string, unknown> {
  return { guild_id: guildId, token, endpoint };
}

function gatewayPacket(
  event: string,
  data: Record<string, unknown>,
  sequence: number,
): Record<string, unknown> {
  return { op: 0, t: event, s: sequence, d: data };
}

function op4(guildId: string, channelId: string | null): DiscordGatewayVoicePayload {
  return {
    op: 4,
    d: {
      guild_id: guildId,
      channel_id: channelId,
      self_mute: false,
      self_deaf: true,
    },
  };
}

async function flushMicrotasks(): Promise<void> {
  for (let index = 0; index < 8; index += 1) await Promise.resolve();
}

async function waitFor(findValue: () => boolean | undefined): Promise<void> {
  const deadline = Date.now() + 2_000;
  while (Date.now() < deadline) {
    if (findValue()) return;
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  throw new Error('timed out waiting for the expected voice state');
}

test('intentional movement preserves paused playback and supersedes pending destinations', async () => {
  const h = createHarness();
  const session = h.session();
  const joined = session.connect();
  h.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  h.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 2));
  await joined;
  await session.play({ file: 'fixture.wav' });
  await session.pause();
  const generation = session.generation;
  const first = session.move('234567890123456780');
  const rejected = assert.rejects(first, /superseded/);
  const channelId = '234567890123456781';
  const second = session.move(channelId);
  h.receive(
    gatewayPacket('VOICE_STATE_UPDATE', { ...voiceState('session'), channel_id: channelId }, 3),
  );
  h.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('new-token'), 4));
  await second;
  await rejected;
  assert.equal(session.channelId, channelId);
  assert.equal(session.state, 'paused');
  assert.equal(session.generation, generation);
  assert.equal(h.connections.length, 1);
  assert.equal(h.replacements.at(-1)?.options.channelId, channelId);
  assert.equal(h.releasedVoiceIds.length, 0);
  const sent = h.sent.length;
  await session.move(channelId);
  assert.equal(h.sent.length, sent);
  await session.dispose();
  assert.equal(h.releasedVoiceIds.length, 1);
});

test('voice flags persist across moves and unchanged values send nothing', async () => {
  const h = createHarness();
  const session = h.session();
  await session.setSelfDeaf(false);
  await session.setSelfMute(true);
  assert.equal(h.sent.length, 0);
  const joined = session.connect();
  assert.equal(h.sent.at(-1)?.payload.d.self_deaf, false);
  assert.equal(h.sent.at(-1)?.payload.d.self_mute, true);
  h.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  h.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 2));
  await joined;
  await session.setSelfDeaf(true);
  const count = h.sent.length;
  await session.setSelfDeaf(true);
  assert.equal(h.sent.length, count);
  const move = session.move('234567890123456780');
  assert.equal(h.sent.at(-1)?.payload.d.self_deaf, true);
  assert.equal(h.sent.at(-1)?.payload.d.self_mute, true);
  const rejected = assert.rejects(move, /disconnect/);
  await session.dispose();
  await rejected;
  assert.equal(h.releasedVoiceIds.length, 1);
});

test('initial processing precedes one-shot playback and moving does not reopen its input', async () => {
  const h = createHarness();
  const session = h.session({ ...sessionOptions, volume: 0.6, filterGraph: 'anull' });
  const joined = session.connect();
  h.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  h.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 2));
  await joined;
  const stream = new PassThrough();
  await session.play({ stream });
  assert.deepEqual(h.operations, ['filter', 'volume', 'input', 'play']);
  const channelId = '234567890123456780';
  const moved = session.move(channelId);
  h.receive(
    gatewayPacket('VOICE_STATE_UPDATE', { ...voiceState('session'), channel_id: channelId }, 3),
  );
  h.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('fresh'), 4));
  await moved;
  assert.equal(session.state, 'playing');
  assert.equal(session.volume, 0.6);
  assert.equal(session.filters.graph, 'anull');
  assert.deepEqual(h.operations, ['filter', 'volume', 'input', 'play']);
  await session.dispose();
  assert.equal(stream.destroyed, false);
  assert.equal(stream.listenerCount('readable'), 0);
  assert.equal(stream.listenerCount('error'), 0);
});

test('failed intentional move retains playback for retry without automatic recovery failure', async () => {
  let fail = true;
  const h = createHarness(undefined, undefined, async () => {
    if (fail) throw new Error('move failed');
  });
  const session = h.session();
  const joined = session.connect();
  h.receive(gatewayPacket('VOICE_STATE_UPDATE', voiceState('session'), 1));
  h.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('token'), 2));
  await joined;
  await session.play({ file: 'fixture.wav' });
  const channelId = '234567890123456780';
  const moved = session.move(channelId);
  const failure = assert.rejects(moved, /move failed/);
  h.receive(
    gatewayPacket('VOICE_STATE_UPDATE', { ...voiceState('session'), channel_id: channelId }, 3),
  );
  h.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('fresh'), 4));
  await failure;
  assert.equal(session.connectionState, 'failed');
  assert.equal(session.state, 'playing');
  assert.deepEqual(h.failedRecoveries, []);
  fail = false;
  const retry = session.connect();
  h.receive(
    gatewayPacket('VOICE_STATE_UPDATE', { ...voiceState('session'), channel_id: channelId }, 5),
  );
  h.receive(gatewayPacket('VOICE_SERVER_UPDATE', voiceServer('retry'), 6));
  await retry;
  assert.equal(h.connections.length, 1);
  await session.dispose();
});
