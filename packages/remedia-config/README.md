# @remedia/config

Reusable Zod 4 runtime validation and generated JSON Schema for the Rust Remedia
server configuration. The package is provided in this workspace; this change does
not publish it.

```ts
import { RemediaConfigSchema, type RemediaConfig } from '@remedia/config';

const config: RemediaConfig = {
  users: [{ username: 'bot', password: "env('REMEDIA_RPC_PASSWORD')" }],
};
const validated = RemediaConfigSchema.parse(config);
```

Parsing applies current defaults but never resolves environment references or
opens media directories. Rust independently validates startup configuration,
resolves supported references, checks resolved usernames/hosts, and canonicalizes
allowed file roots. Unknown keys are rejected at every object level. Input types
allow optional defaulted fields; the schema output has expanded defaults.

Generate artifacts with `pnpm --filter @remedia/config schema`; verify freshness
with `pnpm --filter @remedia/config schema:check`. The exported
`generateJsonSchema()` uses Zod's native draft 2020-12 conversion. The package
exports `@remedia/config/schema.json`; the identical docs artifact is
`apps/docs/public/schemas/remedia.config.schema.json`, deployed at
`https://twlite.github.io/remedia/schemas/remedia.config.schema.json` with the docs.
No server fetch or Node.js runtime dependency is introduced by `$schema` metadata.

JSON Schema describes types, defaults, enums, numeric bounds, and strict objects.
Custom Zod refinements (UTF-8 byte lengths, unique usernames, cross-field capacity,
normalized host syntax/wildcards, and environment-reference syntax) are not fully
representable in the generated editor schema. Editors supplement runtime validation.
Resolved environment-value collisions and filesystem access remain Rust-only checks.
Errors from ordinary schema validation do not include resolved environment secrets.

Shared validation cases and expanded defaults are independently checked by Vitest
and Rust tests. CI checks schema freshness and runs Rust checks on configuration
package changes to preserve this parity.
