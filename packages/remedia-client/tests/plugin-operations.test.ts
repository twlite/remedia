import { describe, expect, expectTypeOf, it } from 'vitest';
import { RemediaClient } from '../lib/client.js';
import {
  NodeSelectionError,
  NodeUnavailableError,
  PluginOperationsUnsupportedError,
  ProtocolError,
  RemediaRpcError,
} from '../lib/errors.js';
import type { PluginExecuteResult, PluginHostDescriptor } from '../lib/types.js';
import { FakeRpcFailure, FakeServerNode, type FakePluginHost } from './helpers.js';

const BOT_ID = '100000000000000001';
const GUILD_ID = '200000000000000001';
const CHANNEL_ID = '300000000000000001';

const metadata = (hostId: string, operations = ['catalog.search', 'lyrics.search']) => ({
  hostId,
  operations: operations.map((operation) => ({ pluginId: 'example.metadata', operation })),
});

interface Fixture {
  client: RemediaClient;
  servers: Record<string, FakeServerNode>;
  joined: Promise<void>;
  close(): Promise<void>;
}

async function fixture(
  nodes: Record<string, { hosts?: FakePluginHost[]; forwarding?: boolean; hostIds?: string[] }>,
  extra: { embedded?: string[] } = {},
): Promise<Fixture> {
  const servers: Record<string, FakeServerNode> = {};

  for (const [id, config] of Object.entries(nodes)) {
    servers[id] = await FakeServerNode.listen({
      id,
      ...(config.forwarding === false
        ? { pluginHosts: config.hostIds ?? [] }
        : { pluginForwarding: { hosts: config.hosts ?? [] } }),
    });
  }

  let joined!: () => void;
  const sent = new Promise<void>((resolve) => (joined = resolve));
  const client = await RemediaClient.create({
    botUserId: BOT_ID,
    nodes: [
      ...Object.entries(servers).map(([id, server]) => ({
        type: 'remote' as const,
        id,
        url: server.url,
        auth: { username: 'bot', password: 'secret' },
      })),
      ...(extra.embedded ?? []).map((id) => ({ type: 'embedded' as const, id })),
    ],
    send: () => joined(),
    requestTimeoutMs: 2_000,
  });

  return {
    client,
    servers,
    joined: sent,
    async close() {
      await client.close();
      await Promise.all(Object.values(servers).map((server) => server.close()));
    },
  };
}

async function connectGuild(context: Fixture, nodeId: string) {
  const voice = context.client.createDiscordSession({
    guildId: GUILD_ID,
    channelId: CHANNEL_ID,
    userId: BOT_ID,
    nodeId,
  });
  const connecting = voice.connect({ timeoutMs: 2_000 });

  await context.joined;
  context.client.receive({
    op: 0,
    t: 'VOICE_STATE_UPDATE',
    s: 1,
    d: { guild_id: GUILD_ID, user_id: BOT_ID, channel_id: CHANNEL_ID, session_id: 'voice-session' },
  });
  context.client.receive({
    op: 0,
    t: 'VOICE_SERVER_UPDATE',
    s: 2,
    d: { guild_id: GUILD_ID, token: 'voice-token', endpoint: 'voice.example.discord.media:443' },
  });
  await connecting;

  return voice;
}

async function waitFor(condition: () => boolean, message: string): Promise<void> {
  const deadline = Date.now() + 5_000;

  while (!condition()) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${message}`);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

const base = {
  nodeId: 'a',
  hostId: 'host-a',
  pluginId: 'example.metadata',
  operation: 'catalog.search',
  input: { query: 'some query' },
} as const;

describe('client.plugins discovery', () => {
  it('lists the hosts and permitted operations of an explicitly selected node', async () => {
    const context = await fixture({ a: { hosts: [metadata('host-a')] } });

    try {
      const hosts = await context.client.plugins.list({ nodeId: 'a' });

      expect(hosts).toEqual([
        {
          nodeId: 'a',
          hostId: 'host-a',
          status: 'healthy',
          operations: [
            { pluginId: 'example.metadata', operation: 'catalog.search' },
            { pluginId: 'example.metadata', operation: 'lyrics.search' },
          ],
        },
      ]);
      expectTypeOf(hosts).toEqualTypeOf<readonly PluginHostDescriptor[]>();
      expect(context.client.nodes[0]?.capabilities?.pluginOperations).toEqual({
        available: true,
        hosts: ['host-a'],
        forwarding: true,
      });
      expect(context.servers.a?.pluginRequests.map((request) => request.command)).toEqual([
        'plugin.list',
      ]);
    } finally {
      await context.close();
    }
  });

  it('combines healthy nodes with different hosts, tagging each host with its node', async () => {
    const context = await fixture({
      zulu: { hosts: [metadata('shared-name', ['b.op'])] },
      alpha: { hosts: [metadata('host-b'), metadata('host-a')] },
      legacy: { forwarding: false, hostIds: ['legacy-host'] },
    });

    try {
      const hosts = await context.client.plugins.list();

      expect(hosts.map((host) => `${host.nodeId}/${host.hostId}`)).toEqual([
        'alpha/host-a',
        'alpha/host-b',
        'zulu/shared-name',
      ]);
      // The older node is neither queried nor treated as an error.
      expect(context.servers.legacy?.pluginRequests).toEqual([]);
    } finally {
      await context.close();
    }
  });

  it('uses the sticky node of a guild and never creates sessions or assignments', async () => {
    const context = await fixture({
      a: { hosts: [metadata('host-a')] },
      b: { hosts: [metadata('host-b')] },
    });

    try {
      await connectGuild(context, 'b');

      const created = context.servers.b!.guildCreateRequests;
      const hosts = await context.client.plugins.list({ guildId: GUILD_ID });

      expect(hosts.map((host) => host.nodeId)).toEqual(['b']);
      await expect(
        context.client.plugins.list({ guildId: GUILD_ID, nodeId: 'a' }),
      ).rejects.toBeInstanceOf(NodeSelectionError);

      // An unassigned guild does not get an assignment from a metadata call.
      const other = '200000000000000002';

      await context.client.plugins.list({ nodeId: 'a', guildId: other });
      expect(context.client.getAssignment(other)).toBeUndefined();
      expect(context.client.assignments.map((assignment) => assignment.guildId)).toEqual([
        GUILD_ID,
      ]);
      expect(context.servers.a!.guildCreateRequests).toBe(0);
      expect(context.servers.b!.guildCreateRequests).toBe(created);
    } finally {
      await context.close();
    }
  });
});

describe('client.plugins execution', () => {
  it('returns typed handled results, including falsy JSON values', async () => {
    const context = await fixture({ a: { hosts: [metadata('host-a')] } });

    try {
      const server = context.servers.a!;

      for (const value of [null, false, 0, '', [], {}, { items: [] }]) {
        server.pluginExecute = () => ({ handled: true, result: value });

        const outcome = await context.client.plugins.execute({ ...base });

        expect(outcome).toEqual({
          handled: true,
          result: value,
          nodeId: 'a',
          hostId: 'host-a',
          pluginId: 'example.metadata',
          operation: 'catalog.search',
        });
      }

      server.pluginExecute = (params) => ({
        handled: true,
        result: { items: [{ title: String((params.input as { query: string }).query) }] },
      });

      const typed = await context.client.plugins.execute<{ items: Array<{ title: string }> }>({
        ...base,
      });

      expectTypeOf(typed).toEqualTypeOf<PluginExecuteResult<{ items: Array<{ title: string }> }>>();

      if (typed.handled) expect(typed.result.items[0]?.title).toBe('some query');
      else throw new Error('expected a handled result');

      // The wire request addresses exactly the requested host and carries opaque input.
      expect(server.pluginRequests.at(-1)).toMatchObject({
        command: 'plugin.execute',
        params: {
          hostId: 'host-a',
          pluginId: 'example.metadata',
          operation: 'catalog.search',
          input: { query: 'some query' },
        },
      });
    } finally {
      await context.close();
    }
  });

  it('reports an unhandled operation as a result, not an exception', async () => {
    const context = await fixture({ a: { hosts: [metadata('host-a')] } });

    try {
      context.servers.a!.pluginExecute = () => ({ handled: false });

      const outcome = await context.client.plugins.execute({ ...base });

      expect(outcome).toEqual({
        handled: false,
        nodeId: 'a',
        hostId: 'host-a',
        pluginId: 'example.metadata',
        operation: 'catalog.search',
      });
      expect('result' in outcome).toBe(false);
    } finally {
      await context.close();
    }
  });

  it('chooses the only node advertising a host and refuses ambiguity', async () => {
    const context = await fixture({
      a: { hosts: [metadata('host-a'), metadata('shared')] },
      b: { hosts: [metadata('host-b'), metadata('shared')] },
    });

    try {
      const { nodeId: _nodeId, ...unpinned } = base;

      expect((await context.client.plugins.execute({ ...unpinned })).nodeId).toBe('a');
      expect((await context.client.plugins.execute({ ...unpinned, hostId: 'host-b' })).nodeId).toBe(
        'b',
      );

      // The same host ID on two nodes may be two different deployments.
      await expect(
        context.client.plugins.execute({ ...unpinned, hostId: 'shared' }),
      ).rejects.toThrow(/several nodes \(a, b\)/);
      expect(
        (await context.client.plugins.execute({ ...unpinned, hostId: 'shared', nodeId: 'b' }))
          .nodeId,
      ).toBe('b');
      await expect(
        context.client.plugins.execute({ ...unpinned, hostId: 'nobody-has-this' }),
      ).rejects.toBeInstanceOf(NodeSelectionError);

      // Only the selected node was contacted for each call: host-a on `a`, host-b and pinned shared on `b`.
      expect(context.servers.a!.pluginRequests).toHaveLength(1);
      expect(context.servers.b!.pluginRequests).toHaveLength(2);
    } finally {
      await context.close();
    }
  });

  it('routes through the sticky assignment of a guild without migrating it', async () => {
    const context = await fixture({
      a: { hosts: [metadata('host-a'), metadata('shared')] },
      b: { hosts: [metadata('host-b'), metadata('shared')] },
    });

    try {
      await connectGuild(context, 'b');

      const { nodeId: _nodeId, ...unpinned } = base;
      const outcome = await context.client.plugins.execute({
        ...unpinned,
        hostId: 'shared',
        guildId: GUILD_ID,
      });

      expect(outcome.nodeId).toBe('b');
      await expect(
        context.client.plugins.execute({ ...base, hostId: 'shared', guildId: GUILD_ID }),
      ).rejects.toBeInstanceOf(NodeSelectionError);
      expect(context.client.getAssignment(GUILD_ID)).toMatchObject({
        nodeId: 'b',
        status: 'assigned',
      });
      expect(context.servers.a!.pluginRequests).toEqual([]);
    } finally {
      await context.close();
    }
  });

  it('reports unsupported nodes without sending a nonexistent command', async () => {
    const context = await fixture(
      {
        old: { forwarding: false, hostIds: ['host-old'] },
        bare: { forwarding: false },
      },
      { embedded: ['local'] },
    );

    try {
      for (const nodeId of ['old', 'bare', 'local']) {
        await expect(
          context.client.plugins.execute({ ...base, nodeId, hostId: 'host-old' }),
        ).rejects.toBeInstanceOf(PluginOperationsUnsupportedError);
        await expect(context.client.plugins.list({ nodeId })).rejects.toBeInstanceOf(
          PluginOperationsUnsupportedError,
        );
      }

      expect(context.servers.old!.pluginRequests).toEqual([]);
      expect(context.servers.bare!.pluginRequests).toEqual([]);
      // Existing operation-source capabilities remain intact on older servers.
      expect(context.client.nodes.find((node) => node.id === 'old')?.capabilities).toMatchObject({
        pluginOperations: { available: true, hosts: ['host-old'], forwarding: false },
      });
      expect(context.client.nodes.find((node) => node.id === 'local')?.capabilities).toMatchObject({
        pluginOperations: { available: false, hosts: [], forwarding: false },
      });
    } finally {
      await context.close();
    }
  });

  it('surfaces server policy and host failures as typed RPC errors', async () => {
    const context = await fixture({ a: { hosts: [metadata('host-a')] } });

    try {
      for (const code of [
        'plugin_operation_not_allowed',
        'plugin_host_unauthorized',
        'plugin_host_unavailable',
        'plugin_result_too_large',
        'request_timeout',
      ]) {
        context.servers.a!.pluginExecute = () => {
          throw new FakeRpcFailure(code);
        };

        await expect(context.client.plugins.execute({ ...base })).rejects.toMatchObject({
          name: 'RemediaRpcError',
          code,
          nodeId: 'a',
        });
      }

      expect(
        await context.client.plugins.execute({ ...base }).catch((error: unknown) => error),
      ).toBeInstanceOf(RemediaRpcError);
    } finally {
      await context.close();
    }
  });

  it('rejects malformed responses as protocol errors', async () => {
    const context = await fixture({ a: { hosts: [metadata('host-a')] } });

    try {
      for (const malformed of [{}, { handled: 'yes' }, { handled: true }, 'text', null]) {
        context.servers.a!.pluginExecute = () => malformed;

        await expect(context.client.plugins.execute({ ...base })).rejects.toBeInstanceOf(
          ProtocolError,
        );
      }
    } finally {
      await context.close();
    }
  });

  it('validates arguments before contacting a node', async () => {
    const context = await fixture({ a: { hosts: [metadata('host-a')] } });

    try {
      const circular: Record<string, unknown> = {};

      circular.self = circular;

      for (const invalid of [
        { hostId: '' },
        { pluginId: '' },
        { operation: '' },
        { input: undefined as never },
        { input: (() => 1) as never },
        { input: circular as never },
        { input: 1n as never },
      ]) {
        await expect(
          context.client.plugins.execute({ ...base, ...invalid }),
        ).rejects.toBeInstanceOf(TypeError);
      }

      await expect(
        context.client.plugins.execute({ ...base, timeoutMs: 0 }),
      ).rejects.toBeInstanceOf(RangeError);
      // `null` is a legitimate JSON input.
      await expect(context.client.plugins.execute({ ...base, input: null })).resolves.toMatchObject(
        { handled: true, result: null },
      );
      expect(context.servers.a!.pluginRequests).toHaveLength(1);
    } finally {
      await context.close();
    }
  });
});

describe('client.plugins cancellation and lifecycle', () => {
  it('cancels an in-flight server operation when the signal aborts', async () => {
    const context = await fixture({ a: { hosts: [metadata('host-a')] } });

    try {
      const server = context.servers.a!;

      server.pluginExecute = () => new Promise(() => undefined);

      const controller = new AbortController();
      const pending = context.client.plugins.execute({ ...base, signal: controller.signal });
      const settled = expect(pending).rejects.toMatchObject({ name: 'AbortError' });

      await waitFor(() => server.pluginPending === 1, 'the request to reach the server');
      controller.abort();
      await settled;

      const request = server.pluginRequests.find((entry) => entry.command === 'plugin.execute');

      await waitFor(() => server.pluginCancels.length === 1, 'the cancel request');
      expect(server.pluginCancels).toEqual([request?.id]);
    } finally {
      await context.close();
    }
  });

  it('does not send a request when the signal is already aborted', async () => {
    const context = await fixture({ a: { hosts: [metadata('host-a')] } });

    try {
      const controller = new AbortController();

      controller.abort();

      await expect(
        context.client.plugins.execute({ ...base, signal: controller.signal }),
      ).rejects.toMatchObject({ name: 'AbortError' });
      await expect(
        context.client.plugins.list({ nodeId: 'a', signal: controller.signal }),
      ).rejects.toMatchObject({ name: 'AbortError' });
      expect(context.servers.a!.pluginRequests).toEqual([]);
    } finally {
      await context.close();
    }
  });

  it('cancels the server operation when the client deadline passes', async () => {
    const context = await fixture({ a: { hosts: [metadata('host-a')] } });

    try {
      const server = context.servers.a!;

      server.pluginExecute = () => new Promise(() => undefined);

      await expect(
        context.client.plugins.execute({ ...base, timeoutMs: 100 }),
      ).rejects.toMatchObject({ code: 'request_timeout' });
      await waitFor(() => server.pluginCancels.length === 1, 'the cancel request');
    } finally {
      await context.close();
    }
  });

  it('fails pending operations on disconnect and works again after reconnecting', async () => {
    const context = await fixture({ a: { hosts: [metadata('host-a')] } });

    try {
      const server = context.servers.a!;

      server.pluginExecute = () => new Promise(() => undefined);

      const pending = context.client.plugins.execute({ ...base });
      const settled = expect(pending).rejects.toBeInstanceOf(NodeUnavailableError);

      await waitFor(() => server.pluginPending === 1, 'the request to reach the server');
      server.closeSockets();
      await settled;

      // While disconnected nothing is queued or sent.
      await expect(context.client.plugins.execute({ ...base })).rejects.toBeInstanceOf(
        NodeUnavailableError,
      );

      server.pluginExecute = undefined;
      await waitFor(() => context.client.nodes[0]?.status === 'healthy', 'reconnection');

      expect(await context.client.plugins.execute({ ...base })).toMatchObject({ handled: true });
      expect(await context.client.plugins.list({ nodeId: 'a' })).toHaveLength(1);
      // Reconnecting recreated the transport session, not any guild session.
      expect(server.guildCreateRequests).toBe(0);
    } finally {
      await context.close();
    }
  });

  it('rejects use after the client is closed', async () => {
    const context = await fixture({ a: { hosts: [metadata('host-a')] } });

    await context.close();
    await expect(context.client.plugins.list()).rejects.toThrow(/closed/);
    await expect(context.client.plugins.execute({ ...base })).rejects.toThrow(/closed/);
  });
});
