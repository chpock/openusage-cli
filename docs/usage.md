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
- Set `--use-daemon=false` to skip daemon discovery and force one-shot local execution
- Falls back to local plugin execution if daemon is unavailable
- For `--type=usage`, fallback performs provider polling and can take noticeable time

Include mode metadata in output:

```bash
openusage-cli query --with-state
```

`state.queryMode` is `cache` (daemon path) or `direct` (fallback path).

### Daemon mode (`run-daemon`)

- Keeps daemon process running and cache populated; a QuickJS runtime is
  created per provider refresh and shared only by discovery and sequential
  probe calls within that refresh
- Serves data through the local REST API
- Also accelerates `query`, because `query` can reuse daemon data

For daemon operation patterns (standalone vs systemd), see [daemon-modes.md](daemon-modes.md).

## Command reference

- `query`: one-shot JSON output (`--type=usage|plugins|config`)
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

`query` flags:

- `--use-daemon[=true|false]` (`true` by default; set `false` to skip daemon discovery)

`run-daemon` flags:

- `--host <host>` (default: `127.0.0.1`)
- `--port <port>` (default: `0`)
- `--refresh-interval-secs <seconds>` (default: `180`)
- `--aggressive-refresh-interval-secs <seconds>` (default: `10`)

  When a provider plan includes a `resetAt` timestamp, the daemon switches to the
  aggressive interval for that provider after the reset time is reached, polling
  more frequently so the new quota snapshot is available promptly.
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

  Response shape (JSON array of objects):

  ```json
  {
    "providerId": "codex",
    "displayName": "Codex",
    "plan": null,
    "lines": [
      {
        "type": "text",
        "label": "Status",
        "value": "ok"
      }
    ],
    "iconUrl": "data:image/svg+xml;base64,...",
    "fetchedAt": "2026-07-30T12:00:00Z",
    "account": {
      "id": "default",
      "origin": "native"
    }
  }
  ```

  The `account` object is present on every usage snapshot. Most providers
  return only `"id": "default"`.

  `account.id` is a provider-scoped unique identifier in output. For current
  bundled plugins, explicit discovery ids are preserved when they are already
  unique. When discovery returns colliding ids, the runtime emits a
  deterministic host-generated id (`acc_v1_<hash>`) to keep output ids unique.

  `account.name` is optional and intended for display labels from discovery.
  It does not participate in routing or identity checks.

  `ctx.account.id` used inside plugin/override probe logic may remain the
  descriptor id even when output `account.id` is canonicalized for uniqueness.

  **`origin` field:** Every account object carries an `origin` string that
  identifies the credential source:

  - `"native"` — credentials supplied by the original plugin (no override active).
  - `"opencode"` — current Codex and Copilot override accounts that read from
    OpenCode auth files.
  - Future releases may introduce additional source identifiers (e.g.
    `"jetbrains"`, `"vscode"`).

  The `origin` is assigned by the runtime based on the account descriptor or
  override context. It is preserved unchanged through the daemon cache and
  serialized API responses.

  When a discovery account has `errorPolicy: "hide-if-other-account"` and at
  least one other account descriptor exists, policy-suppressed probe failures
  (context construction errors, probe exceptions, promise rejections, and
  invalid returned probe objects/lines) are intentionally omitted from output.
  Non-policy errors and unmarked account failures are always included.

  ### Plugin return contract (single-account mode)

  The following applies when the plugin does not export `discoverAccounts`
  (single-account mode). For discovery mode, see
  [Multi-account discovery](#multi-account-discovery).

  The account identity is not determined by the probe result. The runtime
  assigns `{ id: "default", origin: "native" }` on every output. The probe
  returns only `plan` and `lines` — any `account` field in the probe result
  is ignored.

  ### Multi-account discovery

  Plugins may optionally export a `discoverAccounts(ctx)` function that
  returns an array of account descriptors. When present, the runtime
  calls it to determine which accounts to probe. Each discovered account
  is probed sequentially with a fresh child context.

  ```js
  // In plugin.js:
  {
    probe(ctx) { /* ... */ },
    discoverAccounts(ctx) {
      return [
        { id: "work", origin: "native" },
        { id: "personal" }
      ];
    }
  }
  ```

  **Capability detection:** `discoverAccounts` is detected only after the
  plugin script and any override script are evaluated. If absent, `null`,
  or `undefined`, the runtime uses single-account mode with the
  default account.

  If the `discoverAccounts` property accessor throws an exception (e.g. a
  Proxy trap), the runtime produces a single provider-level error output
  with the default account — it does not fall back to single-account mode.

  **Account context:** Each discovered account receives a child context
  that inherits from the base `__openusage_ctx`. The `ctx.account` object
  has an immutable `id` field matching the discovered descriptor. The
  `origin` field is **not** exposed on `ctx.account` — it is a runtime
  output annotation only. In single-account mode, the probe receives a child
  context with `ctx.account = { id: "default" }` whose identity field is
  similarly immutable.

  **Validation:** The returned array must contain 0–32 items. Each item
  must be an object with a non-empty string `id`.
  An optional `errorPolicy` field may be set to `"hide-if-other-account"`
  (see [Discovery error policy](plugin-overrides.md#discovery-error-policy)). An
  optional `origin` string may be provided; if absent, the runtime assigns
  `"native"` as the default. Optional descriptor fields `name`,
  `stableSubjectKey`, and `sourceRef` may also be provided. Violations
  (non-array, missing or empty id, invalid errorPolicy, over 32 items, or an
  exception) produce a single provider-level error output with the default
  account. If effective output ids still collide after host normalization, the
  runtime returns a provider-level duplicate-output-id error.

  **Empty list:** A valid empty array produces zero outputs for that
  provider, clearing any previously cached snapshots.

  **Host-authoritative identity:** In discovery mode, the runtime assigns
  account identity from the discovered descriptor (including `origin`). The
  probe result's `account` field is ignored and never used to override,
  validate, or reject discovered identity.

  **Failure isolation:** A per-account probe failure produces an error
  snapshot carrying that account's identity. Other accounts are unaffected.
  However, when a discovery account has `errorPolicy: "hide-if-other-account"`
  and at least one other account descriptor exists, the policy-suppressed
  failure snapshot is intentionally omitted from output (see
  [Discovery error policy](plugin-overrides.md#discovery-error-policy)).

  **Subscriptions:** All `subscribeFile` calls during `discoverAccounts`
  and each account probe are collected into a single provider-level set.
  File-monitor commands are sent once per provider with the unioned
  subscriptions.

  **Refresh atomicity:** Each provider refresh atomically replaces that
  provider's entire cache vector. A valid empty discovery list clears the
  provider's cached snapshots.

  Vendored plugins do not natively implement `discoverAccounts`, but the
  bundled Codex and Copilot overrides add it. Alongside `default`, they inspect
  these OpenCode auth candidates in stable path-index order:

  1. `~/.local/share/opencode/auth.json` (candidate 0)
  2. `~/.config/opencode/auth.json` (candidate 1)

  A missing candidate file or provider key omits that account. An unreadable or
  malformed candidate produces an account-specific error. Only `default` has
  the `hide-if-other-account` policy; OpenCode account errors remain visible.
  An OpenCode account uses only its selected source and never falls back to a
  different OpenCode file or native credentials.

  Future multi-account providers will add additional items with the same
  `providerId` and a distinct output account `id`. The collection remains a flat
  array — no nesting or grouping changes. Provider filters (e.g.
  `pluginIds=codex`) remain provider-scoped; there is no account selector
  yet.

- `GET /v1/usage/{provider}[?refresh=true]`: usage snapshot for one provider.
  - `404` with `{"error":"provider_not_found"}` for unknown provider.
  - `204 No Content` when provider exists but no cached snapshot is available.

  Response shape is a single object matching the collection item shape above,
  including the `account` field. The single-provider endpoint selects a
  snapshot in this order:
  1. The snapshot with `account.id == "default"` (when present).
  2. Otherwise, the sole cached snapshot if exactly one exists.
  3. Otherwise, `204 No Content` (multiple non-default accounts — account
     selection is deferred; a future account selector will resolve this).

- `GET /v1/config`: return the daemon's active runtime configuration (host, port, refresh intervals, enabled plugins, etc.).
- `POST /v1/probe`: force refresh. Optional JSON body:

  ```json
  {
    "pluginIds": ["codex", "cursor"]
  }
  ```

  Returns the same flat account-bearing array of snapshots as `GET /v1/usage`.

- `query --type=usage` (default mode): prints JSON to stdout. The response
  shape is the same flat array of account-bearing objects described above.
  When `--with-state` is added, the output wraps in `{"state":{...},"data":[...]}`
  and every item in `data` carries the same `account` field.
- `POST /v1/shutdown`: request graceful shutdown.
- `POST /v1/restart`: request daemon restart.

Control endpoint restrictions (`/v1/shutdown`, `/v1/restart`):

- Requests must come from loopback address (`127.0.0.1` / `::1`).
- If `Origin` header is present, it must also be local (`localhost` or loopback IP).
- Non-local remote/origin requests are rejected with `403`.
