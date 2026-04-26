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

Example wrapper:

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
