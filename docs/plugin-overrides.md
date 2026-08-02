# Plugin Overrides

Use plugin overrides to customize plugin behavior without editing `vendor/*`.

## Override directory resolution

Source checkout default lookup:

1. `<repo_root>/plugin-overrides`
2. `<cwd>/plugin-overrides`
3. `<executable_dir>/plugin-overrides`
4. packaged paths

Installed binary lookup:

1. `<prefix>/share/openusage-cli/plugin-overrides`
2. `/usr/share/openusage-cli/plugin-overrides`

## Override file naming

For plugin id `<id>`, first match wins:

1. `<id>.js`
2. `<id>.override.js`
3. `<id>/override.js`

## Runtime helper API

Override scripts run after plugin code and get `globalThis.__openusage_override`:

- `pluginId`
- `originalProbe(ctx)`
- `replaceProbe((ctx, originalProbe) => ...)`
- `wrapProbe((ctx, previousProbe, originalProbe) => ...)`
- `resetProbe()`

### Discovery helpers (reference)

When a plugin exports `discoverAccounts(ctx)`, the runtime calls it to
determine which accounts to probe. The discovery function receives the base
context (without `ctx.account`). Account descriptors are returned in
array order; that order is preserved throughout probing and cache
storage. The function may return a Promise for async discovery.

The core stores only public account identity (`id`, `origin`, optional `name`).
It never stores credentials, provider-specific metadata, or internal
discovery fields such as `errorPolicy`.

- `originalDiscoverAccounts` — the original bound function, or `null` if
  the plugin had none. This is a read-only reference; to replace
  discoverAccounts, use the
  [Declarative function overrides](#declarative-function-overrides)
  manifest instead.

### Account context

In discovery mode, each discovered account gets a fresh child context
inheriting from the base `__openusage_ctx`. Its `ctx.account.id` field
is immutable and matches the discovered account descriptor. The `origin`
field is **not** exposed on `ctx.account` — it is a runtime output
annotation only.

In single-account mode (no `discoverAccounts`), the probe still receives a child
context with `ctx.account = { id: "default" }` whose identity field is
immutable.

### Account descriptor fields

Account descriptors returned by `discoverAccounts` support the following
fields:

- `id` (required, non-empty string) — probe-time account identifier exposed as
  `ctx.account.id`.
- `origin` (optional string) — identifies the credential source. If absent,
  the runtime assigns `"native"` as the default. Known values:
  - `"native"` — credentials supplied by the original plugin (no override active).
  - `"opencode"` — OpenCode auth file credentials (Codex/Copilot overrides).
  - Future releases may introduce additional identifiers.
- `name` (optional string) — display label propagated to output
  `account.name`.
- `stableSubjectKey` (optional string) — stable identity key for host
  canonicalization when descriptor ids collide.
- `sourceRef` (optional string) — normalized source hint used for host
  canonicalization when descriptor ids collide.
- `originNamespace` (optional string) — additional canonicalization namespace
  (default: `"default"`).
- `errorPolicy` (optional string) — controls error suppression behavior
  (see [Discovery error policy](#discovery-error-policy)).

Output identity rules:

- If discovered descriptor ids are already unique, output `account.id` is
  preserved as-is.
- If descriptor ids collide, the runtime emits deterministic host-generated
  output ids (`acc_v1_<hash>`) to keep output ids unique while preserving
  probe-time `ctx.account.id`.
- If output ids still collide after normalization, the runtime returns a
  provider-level duplicate-output-id error.

### Discovery error policy

Account descriptors may include an optional `errorPolicy` field that
controls error suppression behavior. This field is **never serialized**
into account output, cache, API responses, or `ctx.account`. It is a
private runtime directive only.

Supported values:

- `"hide-if-other-account"` — if this account's probe fails and the
  discovery yielded at least one other account descriptor, the error
  snapshot is suppressed. If it succeeds, the result is retained. If
  no other descriptors exist, the error is retained.
- Any other string value or a non-string value is a discovery validation
  error and produces a provider-level error output.

Example:

```js
discoverAccounts(ctx) {
  return [
    { id: "primary" },
    { id: "secondary", errorPolicy: "hide-if-other-account" }
  ];
}
```

Override authors should use this policy for optional credential sources
whose failure should not produce visible errors when other sources are
available.

- If the `discoverAccounts` property accessor throws an exception (e.g.
  a Proxy trap), the runtime produces a single provider-level error
  output with the default account — it does not fall back to single-account mode.
- If `discoverAccounts` is present but not a function, the runtime
  produces a single provider-level error output with the default account.
- If `discoverAccounts` throws or returns a non-array, the runtime
  produces a single provider-level error output.
- If `discoverAccounts` returns descriptors with missing/empty id,
  invalid errorPolicy, or exceeds 32 entries, the runtime produces a
  single provider-level error output.
- If normalized output account ids collide, the runtime produces a single
  provider-level duplicate-output-id error.
- A valid empty array produces zero outputs (no probe calls).
- Per-account probe failures produce an error snapshot carrying that
  account's identity; other accounts are unaffected. When a discovery
  account has `errorPolicy: "hide-if-other-account"` and at least one
  other account descriptor exists, the policy-suppressed failure snapshot
  is intentionally omitted from output (see
  [Discovery error policy](#discovery-error-policy)).
- In discovery mode, account identity is host-authoritative and comes from the
  discovered descriptor. The probe's returned `account` field is ignored and
  never used for identity validation.

### Subscriptions

All `subscribeFile` calls made during `discoverAccounts` and each
account probe are collected into a single provider-level subscription
set. File-monitor commands (`ClearProvider`/`ReplaceProvider`) are
sent once per provider with the unioned subscriptions.

### Refresh and cache

Each provider refresh atomically replaces that provider's entire cache
vector. A valid empty discovery list clears the provider's cached
snapshots. The bundled Codex and Copilot overrides use `discoverAccounts`
for multi-account discovery alongside the single default account.

## Example wrapper

```js
// plugin-overrides/codex.js
globalThis.__openusage_override.wrapProbe(function (ctx, previousProbe, originalProbe) {
  return previousProbe(ctx)
})
```

## AST patching (advanced)

You can patch non-exported plugin internals before `eval` by defining `globalThis.__openusage_ast_patch`:

```js
globalThis.__openusage_ast_patch = {
  functions: [
    { target: "loadAuth", with: "patchLoadAuth", mode: "wrap" },
    { target: "saveAuth", with: "patchSaveAuth", mode: "wrap" },
  ],
}

function patchLoadAuth(originalLoadAuth, ctx) {
  return originalLoadAuth(ctx)
}

function patchSaveAuth(originalSaveAuth, ctx, authState) {
  return originalSaveAuth(ctx, authState)
}
```

When patching is applied, original functions are renamed to `__openusage_original_<target>`.

For `mode: "wrap"`, the callback receives the renamed original function as its first
argument, followed by the target function's original arguments. For `mode: "replace"`,
the callback receives only the target function's original arguments. The `with` value
must name a callback available as `globalThis[with]` when the patched function runs.

`globalThis.__openusage_override` remains available for normal override helpers such as
`originalProbe`, `replaceProbe`, `wrapProbe`, and `resetProbe`.

## Declarative function overrides

Override scripts may declare function replacements declaratively via
`globalThis.__openusage_function_overrides`, placed alongside
`__openusage_ast_patch`. The manifest is processed after override evaluation
and uses the existing specific discovery override mechanism under the hood.

```js
globalThis.__openusage_function_overrides = {
  functions: [
    { target: "discoverAccounts", with: "discoverAccounts", mode: "replace" }
  ]
};
```

**Whitelist:** The manifest currently accepts only the following values.
Invalid descriptors produce a visible error at startup:

| Field    | Allowed values               |
|----------|------------------------------|
| `target` | `"discoverAccounts"`         |
| `mode`   | `"replace"`                  |
| `with`   | any callable global function |

**Processing behavior:**

- **Absent manifest** (`null`, `undefined`, or missing): no-op.
- **Malformed manifest** (missing `functions` field, non-object, non-array):
  fails with a detailed error.
- **Invalid `target`** (anything other than `"discoverAccounts"`): fails with a
  visible error.
- **Invalid `mode`** (anything other than `"replace"`): fails with a visible error.
- **Missing or non-callable `with` reference**: fails with a visible error.

Each valid entry installs the named `with` function as the `discoverAccounts`
replacement. The callback receives `(ctx, originalDiscoverAccounts)` — matching
the existing override contract.
