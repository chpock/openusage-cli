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
- Publishes per-user daemon discovery files for local client auto-connect

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
openusage-cli --host 127.0.0.1 --port 0
```

Default bind port is `0` (random free port assigned by OS).

CLI options:

- `--plugins-dir <path>` (or `OPENUSAGE_PLUGINS_DIR`)
- `--enabled-plugins <csv-globs>` (or `OPENUSAGE_ENABLED_PLUGINS`, default: `*`)
- `--port <port>` (default: `0` = random free port)
- `--app-data-dir <path>` (or `OPENUSAGE_APP_DATA_DIR`)
- `--plugin-overrides-dir <path>` (or `OPENUSAGE_PLUGIN_OVERRIDES_DIR`)
- `--refresh-interval-secs <seconds>` (default: `300`)
- `--default-config` (print default `config.yaml` template to stdout and exit)
- `--daemon[=true|false]` (when enabled, spawn background process and exit parent; default value for bare flag is `true`)
- `--existing-instance <error|ignore|replace>` (default: `error`; controls behavior when a running daemon is already discovered)
- `--service-mode <standalone|systemd>` (default: `standalone`; mainly for service managers)
- `--install-systemd` (create `~/.config/systemd/user/openusage-cli.service` for current user with `ExecStart=... --daemon=false --service-mode=systemd`)

Default plugin auto-discovery order (when `--plugins-dir` is not set):

When running from source (`openusage-cli` from `target/{debug,release}`):

1. `<repo_root>/vendor/openusage/plugins`
2. `<repo_root>/plugins`
3. `<executable_dir>/vendor/openusage/plugins`
4. `<executable_dir>/plugins`
5. `<prefix>/share/openusage-cli/openusage-plugins` (derived from executable path)
6. `/usr/share/openusage-cli/openusage-plugins`

When running as an installed binary (Linux/FHS layout):

1. `<prefix>/share/openusage-cli/openusage-plugins` (derived from executable path, e.g. `/usr/bin` -> `/usr/share`)
2. `/usr/share/openusage-cli/openusage-plugins`

By default, the app runs in console mode and logs to stdout/stderr. Stop it with `Ctrl+C`.

## Daemon Discovery File

To help other local applications auto-discover the running daemon, `openusage-cli` publishes one runtime file per user:

- `daemon-endpoint` - plain-text full daemon URL including scheme (for example, `http://127.0.0.1:6737` or `http://[::1]:6737`)

Path resolution for discovery files:

1. `ProjectDirs::runtime_dir()/runtime` (preferred)
2. fallback: `ProjectDirs::data_local_dir()/runtime`
3. fallback when `ProjectDirs` is unavailable: `./.openusage-cli/runtime`

Current filename inside that directory:

- `daemon-endpoint`

Lifecycle behavior:

- File is written atomically after HTTP bind succeeds.
- File is removed on graceful shutdown.
- If daemon binds to `0.0.0.0` or `::`, published endpoint is normalized to localhost for client connections.
- When `--existing-instance=ignore` is set, discovery file publication is disabled.

Recommended client flow:

1. Read `daemon-endpoint` from the discovery path.
2. Use its content as base URL.
3. Call `GET /health` to verify liveness, then query other API endpoints.

## Configuration File

- Config path is resolved via `ProjectDirs::from("com", "openusage", "openusage-cli")` as `config_dir()/config.yaml`, with fallback to `./.openusage-cli/config.yaml`.
- If the file is missing, daemon startup continues with CLI/env/default values (no auto-create).
- To print a full default config template with comments, run:

```bash
openusage-cli --default-config
```

## Plugin Overrides (without editing `vendor/*`)

`openusage-cli` can load per-plugin override scripts from a separate directory.

- When running from source, default lookup is `<repo_root>/plugin-overrides`, then `<executable_dir>/plugin-overrides`, then packaged paths.
- When running installed binary, lookup is `<prefix>/share/openusage-cli/plugin-overrides`, then `/usr/share/openusage-cli/plugin-overrides`.
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

## Build Linux Packages

Install helper cargo subcommands once:

```bash
cargo install cargo-deb cargo-generate-rpm
```

Build packages:

```bash
make deb
make rpm
# or both
make packages
```

Package layout:

- Binary: `/usr/bin/openusage-cli`
- Upstream plugins: `/usr/share/openusage-cli/openusage-plugins`
- Override plugins: `/usr/share/openusage-cli/plugin-overrides`

## Release Versioning

- `Cargo.toml` keeps a fixed dev version (`0.0.0`) on branches.
- Production release version comes from the git tag (`vX.Y.Z`) in GitHub Actions.
- The release workflow validates tag format, injects tag version into `Cargo.toml` for packaging, and exports `OPENUSAGE_BUILD_VERSION` so the binary reports the tagged version.
- To avoid tag typos locally, use:

```bash
make release-tag VERSION=0.2.0
git push origin v0.2.0
```
