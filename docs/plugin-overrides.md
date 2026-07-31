# Plugin Overrides

Use plugin overrides to customize plugin behavior without editing `vendor/*`.

## Override directory resolution

Source checkout default lookup:

1. `<repo_root>/plugin-overrides`
2. `<executable_dir>/plugin-overrides`
3. packaged paths

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
- `wrapProbe((ctx, currentProbe, originalProbe) => ...)`
- `resetProbe()`

### Discovery helpers (optional)

When a plugin exports `discoverAccounts(ctx)`, the runtime calls it to
determine which accounts to probe. Override scripts can intercept this
with the following helpers. The discovery function receives the base
context (without `ctx.account`). Account descriptors are returned in
array order; that order is preserved throughout probing and cache
storage. The function may return a Promise for async discovery.

The core stores only public account identity (`id`, `displayName`).
It never stores credentials or provider-specific metadata.

- `originalDiscoverAccounts` — the original bound function, or `null` if
  the plugin had none.
- `replaceDiscoverAccounts((ctx, originalDiscoverAccounts) => [...])` —
  may add discovery to a legacy plugin that lacks it. The replacement
  receives the base context and the original (or `null`) and must return
  an array of account descriptors.
- `wrapDiscoverAccounts((ctx, currentDiscoverAccounts, originalDiscoverAccounts) => [...])` —
  requires a current discovery function (native or previously replaced).
  The wrapper receives the base context, the current (bound) function,
  and the original (or `null`). Throws if no current discovery function
  exists.
- `resetDiscoverAccounts()` — restores the original discovery method;
  if none originally existed, removes the method added by the override.

### Discovery helper examples

```js
// Add discovery to a legacy plugin
globalThis.__openusage_override.replaceDiscoverAccounts(function(ctx, original) {
  return [{ id: "work", displayName: "Work Account" }];
});

// Wrap native discovery to append an account
globalThis.__openusage_override.wrapDiscoverAccounts(async function(ctx, current, original) {
  var accounts = await current(ctx);
  accounts.push({ id: "extra", displayName: "Extra Account" });
  return accounts;
});
```

### Account context

In discovery mode, each discovered account gets a fresh child context
inheriting from the base `__openusage_ctx`. Its `ctx.account.id` and
`ctx.account.displayName` fields are immutable and match the discovered
account descriptor.

In legacy mode (no `discoverAccounts`), the probe still receives a child
context with `ctx.account = { id: "default", displayName: "default" }`
whose identity fields are immutable.

### Error handling

- If the `discoverAccounts` property accessor throws an exception (e.g.
  a Proxy trap), the runtime produces a single provider-level error
  output with the default account — it does not fall back to legacy mode.
- If `discoverAccounts` is present but not a function, the runtime
  produces a single provider-level error output with the default account.
- If `discoverAccounts` throws or returns a non-array, the runtime
  produces a single provider-level error output.
- If `discoverAccounts` returns descriptors with missing/empty fields,
  duplicate ids, or exceeds 32 entries, the runtime produces a single
  provider-level error output.
- A valid empty array produces zero outputs (no probe calls).
- Per-account probe failures produce an error snapshot carrying that
  account's identity; other accounts are unaffected.
- In discovery mode, the probe's returned `account` must match the
  discovered account identity (absent/null/undefined/explicit default means
  the discovered account). A mismatch in either id or displayName produces
  an account-specific error carrying the discovered identity.

### Subscriptions

All `subscribeFile` calls made during `discoverAccounts` and each
account probe are collected into a single provider-level subscription
set. File-monitor commands (`ClearProvider`/`ReplaceProvider`) are
sent once per provider with the unioned subscriptions.

### Refresh and cache

Each provider refresh atomically replaces that provider's entire cache
vector. A valid empty discovery list clears the provider's cached
snapshots. No bundled plugin has adopted `discoverAccounts` yet.

## Example wrapper

```js
// plugin-overrides/codex.js
globalThis.__openusage_override.wrapProbe(function (ctx, currentProbe) {
  return currentProbe(ctx)
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

function patchLoadAuth(original, ctx) {
  return original(ctx)
}

function patchSaveAuth(original, ctx, authState) {
  return original(ctx, authState)
}
```

When patching is applied, original functions are renamed to `__openusage_original_<target>`.
