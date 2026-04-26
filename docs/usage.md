# Usage

## Basic commands

```bash
# one-shot query (default mode)
openusage-cli query

# same as above
openusage-cli

# run daemon (background by default)
openusage-cli run-daemon --host 127.0.0.1 --port 0
```

Default daemon port is `0`, which means "pick a free port".

## Mode behavior

### Query mode (`query`, default)

- Returns JSON payload once, then exits
- Tries daemon first for fast responses
- Falls back to local plugin execution if daemon is unavailable
- For `--type=usage`, fallback performs provider polling and can take noticeable time

Include mode metadata in output:

```bash
openusage-cli query --with-state
```

`state.queryMode` is `cache` (daemon path) or `direct` (fallback path).

### Daemon mode (`run-daemon`)

- Keeps plugin runtime initialized
- Refreshes snapshots in the background (default interval: 300 seconds)
- Serves data through the local REST API
- Also accelerates `query`, because `query` can reuse daemon data

For daemon operation patterns (standalone vs systemd), see `docs/daemon-modes.md`.

## Command reference

- `query`: one-shot JSON output (`--type=usage|plugins`)
- `run-daemon`: start daemon mode
- `show-default-config`: print default `config.yaml` template
- `install-systemd-unit`: create `~/.config/systemd/user/openusage-cli.service`
- `version`: print version
- `help`: print help

## Important flags

Global flags:

- `--log-level <error|warn|info|debug|trace>`

Runtime flags (`query`, `run-daemon`):

- `--plugins-dir <path>` (or `OPENUSAGE_PLUGINS_DIR`)
- `--enabled-plugins <csv-globs>` (or `OPENUSAGE_ENABLED_PLUGINS`, default: `*`)
- `--app-data-dir <path>` (or `OPENUSAGE_APP_DATA_DIR`)
- `--plugin-overrides-dir <path>` (or `OPENUSAGE_PLUGIN_OVERRIDES_DIR`)

`run-daemon` flags:

- `--host <host>` (default: `127.0.0.1`)
- `--port <port>` (default: `0`)
- `--refresh-interval-secs <seconds>` (default: `300`)
- `--existing-instance <error|ignore|replace>` (default: `error`)
- `--service-mode <standalone|systemd>` (default: `standalone`)
- `--foreground[=true|false]` (`--foreground` means `true`, default: `false`)
- `--daemon-child` (internal)

## API endpoints

- `GET /health`
- `GET /v1/plugins`
- `GET /v1/usage`
- `GET /v1/usage/{provider}`
- `POST /v1/probe`
- `GET /v1/config`
- `POST /v1/shutdown`
- `POST /v1/restart`
