import { Readable } from 'node:stream';
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  createMediaSource,
  createPluginHost,
  PluginHostError,
  type Operation,
  type PluginHost,
} from '../src/index.js';
const hosts: PluginHost[] = [];
function host(options: Partial<Parameters<typeof createPluginHost>[0]> = {}) {
  const h = createPluginHost({
    name: 'Test',
    publicUrl: 'http://localhost',
    onError: vi.fn(),
    ...options,
  });
  hosts.push(h);
  return h;
}
afterEach(async () => {
  await Promise.all(hosts.splice(0).map((h) => h.close().catch(() => {})));
  vi.useRealTimers();
});
function request(operation = 'echo', input: unknown = null, init: RequestInit = {}) {
  return new Request('http://localhost/remedia/v1/execute', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ pluginId: 'example', operation, input }),
    ...init,
  });
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}
async function grant(h: PluginHost) {
  const response = await h.fetch(request());
  expect(response.status).toBe(200);
  return (await response.json()).result.url as string;
}

describe('direct dispatcher', () => {
  it('preserves arbitrary input/output identity and valid false/null results', async () => {
    const h = host();
    const value = new Map([['x', new Date()]]);
    const output = { value, stream: Readable.from([]) };
    const echo = vi.fn((_input: unknown) => output);
    h.register({
      id: 'example',
      operations: { echo, no: () => false, nil: () => null, decline: () => undefined },
    });
    expect(await h.handle({ pluginId: 'example', operation: 'echo', input: value })).toEqual({
      handled: true,
      result: output,
    });
    expect(echo.mock.calls[0]?.[0]).toBe(value);
    const result = await h.execute('example', 'echo', value);
    expect(result.handled && result.result).toBe(output);
    expect(await h.execute('example', 'no', null)).toEqual({ handled: true, result: false });
    expect(await h.execute('example', 'nil', null)).toEqual({ handled: true, result: null });
    expect(await h.execute('example', 'decline', null)).toEqual({ handled: false });
    output.stream.destroy();
  });
  it('validates IDs, duplicate registration, own operations and snapshots handlers', async () => {
    const h = host();
    const operations = { echo: () => 1 };
    h.register({ id: 'example', operations });
    operations.echo = () => 2;
    expect(() => h.register({ id: 'example', operations })).toThrow(PluginHostError);
    expect(() => h.register({ id: '../bad', operations })).toThrow(TypeError);
    expect(await h.execute('example', 'echo', null)).toEqual({ handled: true, result: 1 });
    await expect(h.execute('example', 'toString', null)).rejects.toMatchObject({
      code: 'operation_not_found',
    });
    await expect(h.execute('missing', 'echo', null)).rejects.toMatchObject({
      code: 'plugin_not_found',
    });
  });
  it('drains before disposal and shares concurrent unregister completion', async () => {
    const h = host();
    const started = deferred<void>();
    const finish = deferred<void>();
    const dispose = vi.fn();
    let signal!: AbortSignal;
    h.register({
      id: 'example',
      operations: {
        echo: async (_, ctx) => {
          signal = ctx.signal;
          started.resolve();
          await finish.promise;
          return Readable.from([]);
        },
      },
      dispose,
    });
    const pending = h.handle({ pluginId: 'example', operation: 'echo', input: null });
    const rejected = expect(pending).rejects.toMatchObject({ code: 'plugin_unregistered' });
    await started.promise;
    const removal = h.unregister('example');
    expect(h.unregister('example')).toBe(removal);
    await Promise.resolve();
    expect(signal.aborted).toBe(true);
    expect(dispose).not.toHaveBeenCalled();
    expect(() => h.register({ id: 'example', operations: { echo: () => null } })).toThrow();
    finish.resolve();
    await removal;
    await rejected;
    expect(dispose).toHaveBeenCalledTimes(1);
    expect(await h.unregister('example')).toBe(false);
  });
  it('combines cancellation scopes and does not call already cancelled handlers', async () => {
    const h = host();
    const handler = vi.fn();
    h.register({ id: 'example', operations: { echo: handler } });
    await expect(
      h.handle({
        pluginId: 'example',
        operation: 'echo',
        input: null,
        signal: AbortSignal.abort(new Error('stop')),
      }),
    ).rejects.toThrow('stop');
    expect(handler).not.toHaveBeenCalled();
  });
  it('keeps plugins concurrent and waits for active operations on idempotent shutdown', async () => {
    const h = host();
    const ready = deferred<void>();
    const release = deferred<void>();
    const dispose = vi.fn();
    h.register({
      id: 'example',
      operations: {
        echo: async () => {
          ready.resolve();
          await release.promise;
        },
      },
      dispose,
    });
    h.register({ id: 'other', operations: { echo: () => 42 } });
    const pending = h.execute('example', 'echo', null);
    const rejected = expect(pending).rejects.toMatchObject({ code: 'host_closed' });
    await ready.promise;
    expect(await h.execute('other', 'echo', null)).toEqual({ handled: true, result: 42 });
    const close = h.close();
    expect(h.close()).toBe(close);
    expect(dispose).not.toHaveBeenCalled();
    release.resolve();
    await close;
    await rejected;
    expect(dispose).toHaveBeenCalledOnce();
    await expect(h.execute('other', 'echo', null)).rejects.toMatchObject({ code: 'host_closed' });
  });
  it('preserves handler failures and aggregates cleanup failures while disposing other plugins', async () => {
    const h = host();
    const error = new Error('failure');
    const dispose = vi.fn();
    h.register({
      id: 'example',
      operations: {
        echo: () => {
          throw error;
        },
      },
      dispose: () => {
        throw error;
      },
    });
    h.register({ id: 'other', operations: { echo: () => null }, dispose });
    await expect(h.execute('example', 'echo', null)).rejects.toBe(error);
    await expect(h.close()).rejects.toBeInstanceOf(AggregateError);
    expect(dispose).toHaveBeenCalledOnce();
  });
});
describe('HTTP transport', () => {
  it('discovers and dispatches through the same handlers', async () => {
    const h = host();
    const echo = vi.fn((input: unknown) => input);
    h.register({ id: 'example', operations: { echo } });
    expect(
      await (await h.fetch(new Request('http://localhost/.well-known/remedia'))).json(),
    ).toMatchObject({ protocolVersion: 1, plugins: [{ id: 'example', operations: ['echo'] }] });
    expect(await (await h.fetch(request('echo', { id: 1 }))).json()).toEqual({
      handled: true,
      result: { id: 1 },
    });
    expect(echo).toHaveBeenCalledOnce();
    expect((await h.fetch(request('missing'))).status).toBe(404);
  });
  it('enforces auth, body size, content type, shape and methods', async () => {
    const h = host({ token: '1234567890123456', maxRequestBytes: 100 });
    const headers = {
      authorization: 'Bearer 1234567890123456',
      'content-type': 'application/json',
    };
    expect((await h.fetch(request())).status).toBe(401);
    expect((await h.fetch(request('echo', null, { headers, body: '{' }))).status).toBe(400);
    expect((await h.fetch(request('echo', null, { headers, body: ' '.repeat(101) }))).status).toBe(
      413,
    );
    expect((await h.fetch(request('echo', null, { headers, body: '{}' }))).status).toBe(400);
    expect(
      (await h.fetch(request('echo', null, { headers: { authorization: headers.authorization } })))
        .status,
    ).toBe(415);
    expect((await h.fetch(new Request('http://localhost/remedia/v1/execute'))).status).toBe(405);
  });
  it('rejects lossy JSON and redacts/reports handler errors', async () => {
    const onError = vi.fn();
    const h = host({ onError });
    const error = new Error('secret');
    h.register({
      id: 'example',
      operations: {
        echo: (input) => input,
        fail: () => {
          throw error;
        },
        date: () => new Date(),
        big: () => 1n,
        nested: () => ({ absent: undefined }),
      },
    });
    for (const operation of ['date', 'big', 'nested'])
      expect((await h.fetch(request(operation))).status).toBe(502);
    const response = await h.fetch(request('fail'));
    expect(await response.text()).not.toContain('secret');
    expect(onError).toHaveBeenCalledWith(error);
  });
  it('cancels body reading during shutdown', async () => {
    const cancel = vi.fn();
    const h = host();
    const response = h.fetch(
      request('echo', null, {
        body: new ReadableStream({ cancel }),
        duplex: 'half',
      } as RequestInit),
    );
    await Promise.resolve();
    await h.close();
    expect((await response).status).toBeGreaterThanOrEqual(400);
    expect(cancel).toHaveBeenCalledOnce();
  });
});
describe('media ownership', () => {
  it('opens lazily, preserves direct identity, redeems once and disposes after EOF', async () => {
    const h = host();
    const dispose = vi.fn();
    const open = vi.fn(() => Readable.from([Buffer.from('encoded')]));
    const source = createMediaSource({ open, dispose, contentType: 'audio/webm' });
    h.register({ id: 'example', operations: { echo: () => source } });
    const direct = await h.execute('example', 'echo', null);
    expect(direct.handled && direct.result).toBe(source);
    const url = await grant(h);
    expect(open).not.toHaveBeenCalled();
    const response = await h.fetch(new Request(url));
    expect(response.headers.get('content-type')).toBe('audio/webm');
    expect(await response.text()).toBe('encoded');
    expect(dispose).toHaveBeenCalledOnce();
    expect((await h.fetch(new Request(url))).status).toBe(404);
  });
  it('expires unopened tokens and enforces capacity without opening sources', async () => {
    vi.useFakeTimers();
    const h = host({ mediaTtlMs: 10, maxMediaTokens: 1 });
    const open = vi.fn();
    const dispose = vi.fn();
    h.register({ id: 'example', operations: { echo: () => createMediaSource({ open, dispose }) } });
    const url = await grant(h);
    expect((await h.fetch(request())).status).toBe(503);
    expect(dispose).toHaveBeenCalledOnce();
    await vi.advanceTimersByTimeAsync(11);
    expect((await h.fetch(new Request(url))).status).toBe(404);
    expect(open).not.toHaveBeenCalled();
    expect(dispose).toHaveBeenCalledTimes(2);
  });
  it.each(['request', 'body', 'unregister', 'close'])(
    'cancels active streams on %s',
    async (mode) => {
      const h = host();
      const cancel = vi.fn();
      const dispose = vi.fn();
      let signal!: AbortSignal;
      h.register({
        id: 'example',
        operations: {
          echo: () =>
            createMediaSource({
              open: (ctx) => {
                signal = ctx.signal;
                return new ReadableStream<Uint8Array>({ cancel });
              },
              dispose,
            }),
        },
      });
      const url = await grant(h);
      const aborter = new AbortController();
      const response = await h.fetch(new Request(url, { signal: aborter.signal }));
      if (mode === 'request') aborter.abort();
      else if (mode === 'body') await response.body!.cancel();
      else if (mode === 'unregister') await h.unregister('example');
      else await h.close();
      await vi.waitFor(() => expect(dispose).toHaveBeenCalledOnce());
      expect(cancel).toHaveBeenCalledOnce();
      if (mode !== 'body') expect(signal.aborted).toBe(true);
    },
  );
  it('waits for delayed opening, cancels late streams and disposes on opening failure', async () => {
    const h = host();
    const ready = deferred<void>();
    const opened = deferred<ReadableStream<Uint8Array>>();
    const dispose = vi.fn();
    const cancel = vi.fn();
    h.register({
      id: 'example',
      operations: {
        echo: () =>
          createMediaSource({
            open: () => {
              ready.resolve();
              return opened.promise;
            },
            dispose,
          }),
      },
    });
    const response = h.fetch(new Request(await grant(h)));
    await ready.promise;
    const removal = h.unregister('example');
    await Promise.resolve();
    expect(dispose).not.toHaveBeenCalled();
    opened.resolve(new ReadableStream({ cancel }));
    await removal;
    expect((await response).status).toBe(503);
    expect(cancel).toHaveBeenCalledOnce();
    expect(dispose).toHaveBeenCalledOnce();
  });
  it('cleans up failed opening and failed stream reads', async () => {
    const h = host();
    const dispose = vi.fn();
    h.register({
      id: 'example',
      operations: {
        echo: () =>
          createMediaSource({
            open: () => {
              throw new Error('open');
            },
            dispose,
          }),
      },
    });
    expect((await h.fetch(new Request(await grant(h)))).status).toBe(502);
    expect(dispose).toHaveBeenCalledOnce();
    await h.unregister('example');
    const release = vi.fn();
    h.register({
      id: 'example',
      operations: {
        echo: () =>
          createMediaSource({
            open: () =>
              new ReadableStream({
                pull(c) {
                  c.error(new Error('read'));
                },
              }),
            dispose: release,
          }),
      },
    });
    const response = await h.fetch(new Request(await grant(h)));
    await expect(response.text()).rejects.toThrow('read');
    expect(release).toHaveBeenCalledOnce();
  });
  it('cleans eager streams when a grant fails and rejects Range without consuming token', async () => {
    const h = host({ publicUrl: undefined } as unknown as Partial<
      Parameters<typeof createPluginHost>[0]
    >);
    const stream = new Readable({ read() {} });
    h.register({ id: 'example', operations: { echo: () => stream } });
    expect((await h.fetch(request())).status).toBe(500);
    expect(stream.destroyed).toBe(true);
    const remote = host();
    remote.register({
      id: 'example',
      operations: { echo: () => Readable.from([Buffer.from('ok')]) },
    });
    const url = await grant(remote);
    expect((await remote.fetch(new Request(url, { headers: { range: 'bytes=0-' } }))).status).toBe(
      416,
    );
    expect(await (await remote.fetch(new Request(url))).text()).toBe('ok');
  });
});
// Compiled along with the suite: deliberate invalid calls must remain errors.
function operationTyping() {
  type Schema = { resolve: Operation<{ id: string }, { title: string }> };
  const h = createPluginHost<Schema>({ name: 'Typed' });
  h.register<Schema>({ id: 'typed', operations: { resolve: (input) => ({ title: input.id }) } });
  const result: Promise<import('../src/index.js').ExecuteResult<{ title: string }>> = h.handle({
    pluginId: 'typed',
    operation: 'resolve',
    input: { id: '1' },
  });
  // @ts-expect-error Invalid input contract
  h.handle({ pluginId: 'typed', operation: 'resolve', input: { id: 1 } });
  // @ts-expect-error Unknown operation
  h.handle({ pluginId: 'typed', operation: 'missing', input: {} });
  return result;
}
void operationTyping;

it('drains asynchronous media disposal before unregister completes', async () => {
  const h = host();
  const disposal = deferred<void>();
  const started = deferred<void>();
  h.register({
    id: 'example',
    operations: {
      echo: () =>
        createMediaSource({
          open: () => new ReadableStream<Uint8Array>(),
          dispose: async () => {
            started.resolve();
            await disposal.promise;
          },
        }),
    },
  });
  const response = await h.fetch(new Request(await grant(h)));
  const cancellation = response.body!.cancel();
  await started.promise;
  let removed = false;
  const removal = h.unregister('example').then(() => {
    removed = true;
  });
  await Promise.resolve();
  expect(removed).toBe(false);
  disposal.resolve();
  await cancellation;
  await removal;
});
it('cancels a locked stream with a pending read and propagates the lifetime signal', async () => {
  const h = host();
  const cancel = vi.fn();
  let signal!: AbortSignal;
  h.register({
    id: 'example',
    operations: {
      echo: () =>
        createMediaSource({
          open: (ctx) => {
            signal = ctx.signal;
            return new ReadableStream<Uint8Array>({ cancel });
          },
        }),
    },
  });
  const abort = new AbortController();
  const response = await h.fetch(new Request(await grant(h), { signal: abort.signal }));
  const reader = response.body!.getReader();
  const read = reader.read();
  const rejected = expect(read).rejects.toThrow();
  abort.abort(new Error('disconnect'));
  await rejected;
  expect(signal.aborted).toBe(true);
  await vi.waitFor(() => expect(cancel).toHaveBeenCalledOnce());
});
it('allows only one concurrent redemption and revokes unopened grants on unregister', async () => {
  const h = host();
  const open = vi.fn(() => Readable.from([Buffer.from('ok')]));
  const dispose = vi.fn();
  h.register({ id: 'example', operations: { echo: () => createMediaSource({ open, dispose }) } });
  const unused = await grant(h);
  const url = await grant(h);
  const responses = await Promise.all([h.fetch(new Request(url)), h.fetch(new Request(url))]);
  expect(responses.map((r) => r.status).sort()).toEqual([200, 404]);
  await responses.find((r) => r.status === 200)!.text();
  await h.unregister('example');
  expect((await h.fetch(new Request(unused))).status).toBe(404);
  expect(open).toHaveBeenCalledOnce();
  expect(dispose).toHaveBeenCalledTimes(2);
});
it('disposes an unopened source on shutdown and reports cleanup failures', async () => {
  const error = new Error('cleanup');
  const h = host();
  const disposed = vi.fn(() => {
    throw error;
  });
  h.register({
    id: 'example',
    operations: {
      echo: () => createMediaSource({ open: () => Readable.from([]), dispose: disposed }),
    },
  });
  await grant(h);
  await expect(h.close()).rejects.toBeInstanceOf(AggregateError);
  expect(disposed).toHaveBeenCalledOnce();
});
it('rejects oversized streaming bodies and cancels their producers', async () => {
  const h = host({ maxRequestBytes: 10 });
  const cancel = vi.fn();
  const body = new ReadableStream<Uint8Array>({
    start(c) {
      c.enqueue(new Uint8Array(11));
    },
    cancel,
  });
  expect(
    (await h.fetch(request('echo', null, { body, duplex: 'half' } as RequestInit))).status,
  ).toBe(413);
  expect(cancel).toHaveBeenCalledOnce();
});
it('awaits asynchronous destruction of unredeemed eager Node streams', async () => {
  const h = host();
  const destroyed = deferred<void>();
  let complete!: (error?: Error | null) => void;
  const stream = new Readable({
    read() {},
    destroy(_error, callback) {
      complete = callback;
      destroyed.resolve();
    },
  });
  h.register({ id: 'example', operations: { echo: () => stream } });
  await grant(h);
  let removed = false;
  const removal = h.unregister('example').then(() => {
    removed = true;
  });
  await destroyed.promise;
  expect(removed).toBe(false);
  complete(null);
  await removal;
  expect(stream.closed).toBe(true);
});
