# Query and Daemon Modes

`openusage-cli` supports two complementary execution models:

- `query` for on-demand reads
- `run-daemon` for always-on background collection

Use both together: keep a daemon running for fast responses, and call `query` from scripts/tools.

## `query` mode in detail

`query` is the default command mode.

```bash
# equivalent commands
openusage-cli query
openusage-cli
```

Behavior:

1. Try to discover a running daemon.
2. If found, request data from daemon HTTP endpoints (`/v1/usage` or `/v1/plugins`).
3. If no daemon is available (or request fails), fall back to local execution.

Fallback implications:

- `--type=usage`: initializes runtime and actively polls providers, so response time is slower.
- `--type=plugins`: returns plugin metadata and typically completes faster than usage polling.

You can inspect which path was used:

```bash
openusage-cli query --with-state
```

`state.queryMode` is:

- `cache` when data came from daemon
- `direct` when local fallback execution was used

## `run-daemon` mode in detail

Daemon mode keeps runtime warm and refreshes snapshots periodically (`--refresh-interval-secs`, default `300`).

Benefits:

- Lower latency for clients that read data frequently
- Continuous background refresh without reinitializing plugins on every read
- Shared data source for multiple local consumers (CLI, dashboards, alerts, widgets)

Start daemon manually:

```bash
openusage-cli run-daemon
```

By default, this starts a background child process and returns control to your shell.

For interactive/debug runs:

```bash
openusage-cli run-daemon --foreground
```

## Two daemon operation approaches

### 1) Standalone daemon (`run-daemon`)

Best for:

- quick local sessions
- testing configuration changes
- development and debugging (`--foreground`)

Pros:

- minimal setup
- easy to launch ad hoc

Cons:

- less robust lifecycle management
- background default mode detaches from terminal output
- no service manager supervision unless you run it under one

### 2) User systemd service (recommended)

Best for:

- daily usage on Linux desktops/workstations
- long-running reliable background service

Install/update the user unit:

```bash
openusage-cli install-systemd-unit
```

Enable and start it:

```bash
systemctl --user daemon-reload
systemctl --user enable --now openusage-cli.service
systemctl --user status openusage-cli.service
```

View logs:

```bash
journalctl --user -u openusage-cli.service -f
```

Pros:

- automatic restart on failure
- standard lifecycle commands (`start`, `stop`, `restart`, `status`)
- centralized logs via journald
- predictable startup and readiness behavior

Cons:

- requires systemd user services
- one-time setup and familiarity with `systemctl --user`

## Configuration for both approaches

Main runtime knobs:

- `host` / `port`
- `refresh_interval_secs`
- `enabled_plugins`
- `plugins_dir`
- `plugin_overrides_dir`
- `app_data_dir`
- `existing_instance`

Set these via CLI flags, environment variables, or `config.yaml`.
For config location and precedence, see `docs/configuration.md`.

## Practical recommendation

- Use systemd user service as your default operational mode.
- Use `query` from scripts and tools to read already-refreshed daemon data quickly.
- Keep fallback behavior as a safety net for environments where daemon is not running.
