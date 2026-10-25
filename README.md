# openusage-cli

`openusage-cli` is a Linux-first backend for AI usage tracking.

It runs provider plugins from [CrossUsage](https://github.com/barramee27/crossusage), collects usage and quota data, and exposes normalized snapshots through a local REST API and CLI.

## Why this project exists

CrossUsage combines a desktop interface with provider integrations. `openusage-cli` reuses its JavaScript plugins and focuses on the backend side only, following a Unix-style design: do one thing well.

That one thing is reliable data collection from AI providers, so other tools can build UI and automation on top.

## What it gives you

- One local source of truth for usage/quota data across multiple AI providers
- Reuse of the CrossUsage plugin ecosystem instead of custom per-provider integrations
- A headless data source for other tools: dashboards, notifications, alerts, scripts, and machine-to-machine integrations via local REST API

## Operating modes

### `query` mode (default)

- Best for ad-hoc checks and scripts
- Tries to read data from a running daemon first (fast path)
- Can skip daemon discovery with `--use-daemon=false` for guaranteed one-shot local execution
- If no daemon is available, falls back to direct local plugin execution
- Fallback is slower because plugin runtime initialization and provider polling happen during command execution

### `run-daemon` mode

- Best for frequent reads and low-latency consumers
- Keeps daemon process running and cache populated; a QuickJS runtime is
  created per provider refresh and shared only by discovery and sequential
  probe calls within that refresh
- Data is then available immediately through REST API or `openusage-cli query`

See [docs/daemon-modes.md](docs/daemon-modes.md) for mode behavior, tradeoffs, and operational guidance.

## Daemon operation choices

- Standalone (`run-daemon`): quick to start manually, good for local/dev sessions
- User systemd service (`install-systemd-unit`): recommended for daily use, process supervision, and persistent lifecycle management

Detailed setup, pros/cons, and configuration are documented in [docs/daemon-modes.md](docs/daemon-modes.md).

## Linux support

`openusage-cli` currently supports Linux only.

## Quick start

Install package:

- Arch Linux (AUR): `yay -S openusage-cli` (or `openusage-cli-git`)
- Debian/Ubuntu: install `.deb` from GitHub Releases via `apt`
- Fedora/RHEL/openSUSE: install `.rpm` from GitHub Releases via `dnf` or `zypper`

Run a one-shot query:

```bash
openusage-cli query
```

Or start daemon mode:

```bash
openusage-cli run-daemon --host 127.0.0.1 --port 6738
```

Then call the API, for example:

```bash
curl http://127.0.0.1:6738/v1/usage
```

## Main commands

- `query` (default): one-shot JSON query (`usage`, `plugins` or `config`)
- `run-daemon`: start daemon mode and expose HTTP API
- `show-default-config`: print default `config.yaml` template
- `install-systemd-unit`: install user systemd unit
- `version`, `help`

## REST API at a glance

- `GET /health`
- `GET /v1/config`
- `GET /v1/plugins`
- `GET /v1/usage`
- `GET /v1/usage/{provider}`
- `POST /v1/probe`
- `POST /v1/shutdown`
- `POST /v1/restart`

## Documentation

- Installation details: [docs/installation.md](docs/installation.md)
- CLI and runtime options: [docs/usage.md](docs/usage.md)
- JSON output format and examples: [docs/json-output.md](docs/json-output.md)
- Query/daemon behavior and systemd operation: [docs/daemon-modes.md](docs/daemon-modes.md)
- Configuration and daemon discovery: [docs/configuration.md](docs/configuration.md)
- Plugin overrides and AST patching: [docs/plugin-overrides.md](docs/plugin-overrides.md)
- Upstream plugin defects and workaround retirement: [docs/upstream-plugin-workarounds.md](docs/upstream-plugin-workarounds.md)
- Development, testing, packaging, releases: [docs/development.md](docs/development.md)
- CrossUsage plugin updates: [docs/crossusage-update-workflow.md](docs/crossusage-update-workflow.md)
- Known intentional behavior differences: [crossusage_differences.md](docs/crossusage_differences.md)

The vendored source revision and synchronization policy are recorded in [docs/crossusage-upstream.md](docs/crossusage-upstream.md).

For upstream project [CrossUsage](https://github.com/barramee27/crossusage) plugin contracts, see vendored upstream docs:

- [vendor/crossusage/docs/plugins/api.md](vendor/crossusage/docs/plugins/api.md)
- [vendor/crossusage/docs/plugins/schema.md](vendor/crossusage/docs/plugins/schema.md)
