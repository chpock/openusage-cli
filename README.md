# openusage-cli

`openusage-cli` is a cross-platform HTTP daemon that executes OpenUsage plugins and exposes usage snapshots over local HTTP.

The project is implemented in Rust with `tokio` + `rquickjs`, and is designed for maximum compatibility with upstream `openusage` plugin contracts.

## Highlights

- Loads vendored plugin manifests and scripts from `vendor/openusage/plugins`
- Runs plugin `probe(ctx)` in a QuickJS runtime compatible with OpenUsage host APIs
- Exposes daemon HTTP endpoints:
  - `GET /health`
  - `GET /v1/plugins`
  - `GET /v1/usage`
  - `GET /v1/usage/{provider}`
  - `POST /v1/probe`
- Keeps an in-memory snapshot cache with periodic background refresh

## Upstream Compatibility Strategy

To minimize divergence from upstream:

- Plugin engine files are copied from upstream into `src/plugin_engine/`
- Upstream references are vendored into `vendor/openusage/`:
  - `README.md`
  - `LICENSE`
  - `plugins/**`
  - `docs/plugins/api.md`
  - `docs/plugins/schema.md`
  - `src-tauri/src/plugin_engine/**`

This allows using upstream plugins with minimal or zero changes.

## Run

```bash
cargo run -- --host 127.0.0.1 --port 6736
```

CLI options:

- `--plugins-dir <path>` (or `OPENUSAGE_PLUGINS_DIR`)
- `--app-data-dir <path>` (or `OPENUSAGE_APP_DATA_DIR`)
- `--plugin-overrides-dir <path>` (or `OPENUSAGE_PLUGIN_OVERRIDES_DIR`)
- `--refresh-interval-secs <seconds>` (default: `300`)
- `--daemon` (spawn background process and exit parent)

By default, the app runs in console mode and logs to stdout/stderr. Stop it with `Ctrl+C`.

## Plugin Overrides (without editing `vendor/*`)

`openusage-cli` can load per-plugin override scripts from a separate directory.

- Default lookup: `./plugin-overrides` then `<executable_dir>/plugin-overrides`
- Override filename patterns (first match wins):
  - `<plugin-id>.js`
  - `<plugin-id>.override.js`
  - `<plugin-id>/override.js`

Override scripts run after upstream plugin code is loaded and receive helper API in `globalThis.__openusage_override`:

- `pluginId`
- `originalProbe(ctx)`
- `replaceProbe((ctx, originalProbe) => ...)`
- `wrapProbe((ctx, currentProbe, originalProbe) => ...)`
- `resetProbe()`

Optional AST patch manifest (for patching internal non-exported plugin functions before `eval`):

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

When AST patching is enabled, the original function is renamed to `__openusage_original_<target>`, and your patch function is called via wrapper.

Example `probe` wrapper skeleton:

```js
// plugin-overrides/codex.js
globalThis.__openusage_override.wrapProbe(function (ctx, currentProbe, originalProbe) {
  // Later you can add custom API-key lookup from your own file here.
  // You can call the original plugin behavior whenever needed:
  return currentProbe(ctx)
})
```

## Compatibility Tests

```bash
cargo test
```

Current tests include:

- vendored plugin loading smoke test
- real runtime probe test for `mock` plugin
- compatibility harness that validates all vendored plugin scripts can register and run `probe(ctx)` in a stubbed OpenUsage-like JS context without missing host APIs
