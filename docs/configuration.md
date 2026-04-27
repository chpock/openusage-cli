# Configuration

## Config file location

`openusage-cli` looks for `config.yaml` at:

1. XDG config directory (typically `~/.config/openusage-cli/config.yaml`)

Missing config file is valid: startup continues with CLI/config/default values.

Print the default template:

```bash
openusage-cli show-default-config
```

## Effective precedence

Configuration sources are merged in strict order:

1. CLI flags
2. `config.yaml`
3. built-in defaults

For operational behavior of `query` and `run-daemon` (including standalone vs systemd user service), see [daemon-modes.md](daemon-modes.md).

## Proxy configuration

Proxy settings apply to outgoing plugin HTTP requests.

Set in `config.yaml`:

```yaml
proxy:
  enabled: true
  url: http://127.0.0.1:7890
```

Notes:

- `proxy.enabled: true` is required for `proxy.url` to be used.
- `proxy.url` can be `http://...`, `https://...`, or `socks5h://...`.
- If config proxy is not enabled/valid, standard proxy environment variables are used: `HTTPS_PROXY`, `https_proxy`, `HTTP_PROXY`, `http_proxy`.
- Local daemon traffic (`localhost`, `127.0.0.1`, `::1`) is excluded from proxying.

## Plugin discovery

If `--plugins-dir` is not set, default discovery order is:

When running from source (`target/{debug,release}`):

1. `<repo_root>/vendor/openusage/plugins`
2. `<repo_root>/plugins`
3. `<executable_dir>/vendor/openusage/plugins`
4. `<executable_dir>/plugins`
5. `<prefix>/share/openusage-cli/openusage-plugins`
6. `/usr/share/openusage-cli/openusage-plugins`

When running as installed Linux binary (FHS layout):

1. `<prefix>/share/openusage-cli/openusage-plugins`
2. `/usr/share/openusage-cli/openusage-plugins`

## Daemon discovery file

The daemon publishes one per-user file for local client auto-discovery:

- `daemon-endpoint` (example content: `http://127.0.0.1:6737`)

Directory resolution:

1. `ProjectDirs::runtime_dir()/runtime`
2. fallback: `ProjectDirs::data_local_dir()/runtime`
3. fallback: `./.openusage-cli/runtime`

Behavior:

- Written atomically after HTTP bind succeeds
- Removed on graceful shutdown
- Normalizes wildcard bind (`0.0.0.0` / `::`) to localhost for client endpoint publication
- Disabled when `--existing-instance=ignore`
