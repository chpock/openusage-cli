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

`query`-specific control:

- `use_daemon: true|false` in `config.yaml` controls whether `query` first tries daemon discovery.
- CLI `--use-daemon` overrides this value when provided.

## Authentication sources

`enabled_auth_sources` selects which discovered credential sources may be
used for account probes. Its keys are `default` and exact plugin IDs:

```yaml
enabled_auth_sources:
  default: [native, opencode]
  codex: [opencode]
  copilot: [native]
```

- `native`: authentication supported by the original plugin, including its
  own file, environment, and keychain mechanisms.
- `opencode`: additional OpenCode authentication supplied by the Codex/Copilot
  overrides, including their `auth.json` and `accounts.json` candidates.

When an original plugin omits `origin`, even its OpenCode file reads count as
`native`. The core uses the declared source, not a directory blacklist.

A plugin entry completely replaces `default`. Plugins without an entry use
`default`; an omitted section or omitted `default` uses the built-in
`[native, opencode]`. An empty list disables all account probes for that
plugin without disabling the plugin itself. List order does not define
priority, and repeated values do not cause repeated probes.

Use plugin IDs from their manifests: the Codex plugin ID is `codex`, while
`openai` is its provider key inside OpenCode credential files. Unknown plugin
IDs are rejected when initializing local execution or the daemon; entries
for discovered but disabled plugins are valid. Unknown source names and
non-list values are configuration errors.

The core matches each account descriptor's `origin` against the selected
list before calling `probe`. An absent `origin` defaults to `native`. Plugins
without `discoverAccounts` have one implicit native account. A plugin with
no accounts matching its list produces no usage snapshots and clears any
previous snapshots on refresh; it does not fall back to a disabled source.

Discovery still runs and may read credential files or register file
subscriptions for excluded sources. Plugin/override evaluation errors and
discovery contract errors remain visible. The full discovery result is
validated, including the 32-account limit, and account IDs are normalized
before source selection so that changing the list does not change IDs of
remaining accounts. Error suppression counts only enabled accounts.

For `query` and `run-daemon`, `--enabled-auth-sources` accepts a YAML/JSON
object and replaces the entire configuration-file object:

```bash
openusage-cli query --use-daemon=false \
  --enabled-auth-sources '{"default":["native","opencode"],"codex":["opencode"]}'
```

`query --type=config` and `GET /v1/config` report the selected object as
`enabledAuthSources`, including the built-in `default` if it was omitted.
When a query uses a running daemon, it uses and reports that daemon's policy;
query CLI flags do not change the daemon's account selection.

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
