import assert from 'node:assert/strict';
import { test } from 'vitest';

import type { NativeRemedia, NativeVoiceOptions } from '../native.js';
import { DiscordSession } from '../lib/discord.js';
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

test('voice server changes request fresh Gateway credentials before replacing the native transport', async () => {
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
  await waitFor(() => (harness.connections.length === 2 ? true : undefined));
  await waitFor(() => (session.connectionState === 'connected' ? true : undefined));

  assert.equal(harness.connections[1]?.token, 'new-token');
  assert.equal(harness.connections[1]?.endpoint, 'voice.example.test:443');
  assert.equal(harness.connections[1]?.sessionId, 'new-session');
  assert.deepEqual(
    harness.sent.map(({ payload }) => payload),
    [
      op4(sessionOptions.guildId, sessionOptions.channelId),
      op4(sessionOptions.guildId, sessionOptions.channelId),
    ],
  );
  assert.deepEqual(harness.disconnectedVoiceIds, ['voice-1']);
  assert.deepEqual(harness.releasedVoiceIds, ['voice-1']);
  await session.disconnect();
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
) {
  const sent: Array<{ guildId: string; payload: DiscordGatewayVoicePayload }> = [];
  const connections: NativeVoiceOptions[] = [];
  const disconnectedVoiceIds: string[] = [];
  const releasedVoiceIds: string[] = [];
  const voiceStates = new Map<string, string>();
  let nextVoiceId = 1;

  const native = {
    connectVoice: async (options: NativeVoiceOptions) => {
      connections.push(options);
      if (connectVoice) return connectVoice(options);
      const voiceId = `voice-${nextVoiceId++}`;
      voiceStates.set(voiceId, 'connected');
      return { voiceId, playbackId: `playback-${voiceId}` };
    },
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
    sent,
    connections,
    disconnectedVoiceIds,
    releasedVoiceIds,
    setVoiceState(voiceId: string, state: string) {
      voiceStates.set(voiceId, state);
    },
    receive(packet: unknown) {
      const dispatch = gateway.receive(packet);
      if (dispatch) {
        sessions
          .get(dispatch.guildId)
          ?.receiveGatewayDispatch(dispatch.event, dispatch.data, dispatch.sequence);
      }
    },
    session(options = sessionOptions) {
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
      self_deaf: false,
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
