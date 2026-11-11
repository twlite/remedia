# @remedia/plugin-host

Build a plugin service for Remedia. A **plugin host** registers plugins, and each plugin exposes named
operations that take JSON input and return a result. `remedia-server` can call a host to play the
audio an operation returns, and your application can run operations through Remedia to get
application-defined data (catalog, metadata, lyrics) without knowing the host's address.

It is framework-independent, has no runtime dependencies, never starts a listener on its own, and does
not sandbox plugins. Requires Node.js 22.18 or newer.

```sh
npm install @remedia/plugin-host
```

A plugin host is its own service. Registering it with Remedia
(`remedia-server plugins install <url>`) records where it is; no plugin code is installed or run by the
server.

## Example

Declare operation inputs and outputs once. Register initialized plugins with explicit schemas; operation names and semantics belong to the application.

```ts
import { createPluginHost, type Operation } from '@remedia/plugin-host';

type Operations = {
  resolve: Operation<{ id: string }, { id: string; title: string }>;
};
const host = createPluginHost<Operations>({ name: 'My Host' });
host.register<Operations>({
  id: 'example.source',
  operations: {
    resolve: async ({ id }, { signal }) => {
      signal.throwIfAborted();
      return { id, title: 'Example' };
    },
  },
});
try {
  const outcome = await host.handle({
    pluginId: 'example.source',
    operation: 'resolve',
    input: { id: '123' },
  });
  if (outcome.handled) console.log(outcome.result.title);
} finally {
  await host.close();
}
```

No HTTP server, serialization, or native Remedia installation is required. Inputs and outputs can contain arbitrary objects, class instances, functions and streams. Only `undefined` means unhandled; `null` and `false` are valid handled results. `execute(pluginId, operation, input, { signal })` provides positional dispatch with unknown input/output types; `handle()` is the typed primary API. Without a schema, `handle()` accepts unknown values. You can alternatively augment the exported `PluginOperations` interface with application-wide operations.

Registration requires a unique ID and at least one own operation. IDs and operation names start with an ASCII letter/digit and contain at most 128 letters, digits, dots, underscores or hyphens. Duplicate IDs throw `PluginHostError` with `duplicate_plugin`. Handlers are copied at registration. Registration does not initialize plugins for you.

## Documentation

- [Plugins overview](https://remedia.twlite.dev/docs/plugins/overview)
- [Creating a plugin host](https://remedia.twlite.dev/docs/server/plugins): HTTP servers, Express, Hono, and media
- [Playing audio through plugins](https://remedia.twlite.dev/docs/server/plugin-operations) and [executing plugin operations](https://remedia.twlite.dev/docs/server/plugin-forwarding)
- [Reference](https://remedia.twlite.dev/docs/plugins/host-reference): HTTP protocol, cancellation, errors
- [Permissions and security](https://remedia.twlite.dev/docs/plugins/security)
