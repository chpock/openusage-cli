# openusage vs openusage-cli: behavior differences

This file documents intentional behavior differences between upstream `openusage` and this repository (`openusage-cli`).

## Environment variable resolution

- **Upstream `openusage` behavior**: `host.env.get(name)` first checks the process environment, then falls back to launching shell commands (for example `printenv` through login/interactive shell modes) to discover variables that are only present in user shell initialization.
- **`openusage-cli` behavior**: `host.env.get(name)` reads from the process environment only. It does not launch a shell and does not run `printenv` fallback commands.

### Why upstream implemented shell fallback

Upstream runs in desktop/Tauri contexts where the app process may not inherit the same environment as an interactive terminal session. Shell fallback helps plugins find variables configured in shell startup files.

### Why `openusage-cli` removed shell fallback

`openusage-cli` is a daemon-first service. Launching shell startup logic during bootstrap/probe can cause operational problems:

- shell init can be slow and delay daemon startup;
- shell init can execute arbitrary user scripts with side effects;
- shell/job-control behavior can interfere with process lifecycle in interactive sessions.

For predictability and safety, `openusage-cli` treats the daemon process environment as the only source of truth.

## Provider file monitoring

- **Upstream `openusage` behavior**: the plugin API does not include a file
  subscription mechanism.  Plugins read local files during each probe via
  `ctx.host.fs.*` synchronous I/O, and no reactive refresh occurs on file
  changes.
- **`openusage-cli` behavior**: extends the plugin API with
  `ctx.host.fs.subscribeFile(path)`.  Plugins can declare exact file
  dependencies.  When a declared file changes, the daemon debounces changes and
  performs a targeted cache refresh for the affected providers only (no daemon
  restart).  The monitor actor is independent of the static restart watcher.

For detailed documentation, see [Provider file monitoring](provider-file-monitoring.md).

## Multi-account discovery

- **Upstream `openusage` behavior**: no account discovery mechanism exists.
  Each plugin runs one probe per refresh and produces a single result.
- **`openusage-cli` behavior**: extends the plugin API with an optional
  `discoverAccounts(ctx)` export. When present, the runtime calls it to
  obtain an array of account descriptors, then probes each account
  sequentially with an inherited context containing an immutable `account`
  identity. Override scripts can intercept, replace, wrap, or reset
  discovery via `globalThis.__openusage_override` helpers.

For detailed documentation, see [Usage](usage.md) (multi-account discovery
section) and [Plugin Overrides](plugin-overrides.md).
