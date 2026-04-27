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

For daemon operation patterns (standalone vs systemd), see [daemon-modes.md](daemon-modes.md).

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

- `--plugins-dir <path>`
- `--enabled-plugins <csv-globs>` (default: `*`)
- `--app-data-dir <path>`
- `--plugin-overrides-dir <path>`

`run-daemon` flags:

- `--host <host>` (default: `127.0.0.1`)
- `--port <port>` (default: `0`)
- `--refresh-interval-secs <seconds>` (default: `300`)
- `--existing-instance <error|ignore|replace>` (default: `error`)
- `--service-mode <standalone|systemd>` (default: `standalone`)
- `--foreground[=true|false]` (`--foreground` means `true`, default: `false`)
- `--daemon-child` (internal)

## API endpoints

Base URL example:

```text
http://127.0.0.1:6738
```

- `GET /health`: service health and loaded plugin count.
- `GET /v1/plugins`: plugin metadata for discovered plugins.
- `GET /v1/usage[?refresh=true][&pluginIds=codex,cursor]`: usage snapshots.
  - `refresh` (optional, default `false`): trigger fresh probe before returning data.
  - `pluginIds` (optional): comma-separated plugin IDs filter.
- `GET /v1/usage/{provider}[?refresh=true]`: usage snapshot for one provider.
  - `404` with `{"error":"provider_not_found"}` for unknown provider.
  - `204 No Content` when provider exists but no cached snapshot is available.
- `POST /v1/probe`: force refresh. Optional JSON body:

```json
{
  "pluginIds": ["codex", "cursor"]
}
```

- `GET /v1/config`: runtime config currently used by daemon.
- `POST /v1/shutdown`: request graceful shutdown.
- `POST /v1/restart`: request daemon restart.

Control endpoint restrictions (`/v1/shutdown`, `/v1/restart`):

- Requests must come from loopback address (`127.0.0.1` / `::1`).
- If `Origin` header is present, it must also be local (`localhost` or loopback IP).
- Non-local remote/origin requests are rejected with `403`.
