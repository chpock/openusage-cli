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

`query` flags:

- `--use-daemon[=true|false]` (`true` by default; set `false` to skip daemon discovery)

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
      "displayName": "default"
    }
  }
  ```

  The `account` object is present on every usage snapshot. Account IDs are
  provider-scoped — each provider defines its own account namespace. Current
  providers always return `"id": "default"`.

  ### Plugin return contract (legacy mode)

  The following applies when the plugin does not export `discoverAccounts`
  (legacy single-probe mode). For discovery mode, see
  [Multi-account discovery](#multi-account-discovery).

  A plugin may return an `account` object inside its probe result alongside
  `plan` and `lines`:

  ```js
  {
    account: { id: "my-account", displayName: "My Account" },
    plan: "Pro plan",
    lines: [/* ... */]
  }
  ```

  - If `account` is absent, `null`, or `undefined`, the runtime assigns
    `{ id: "default", displayName: "default" }`.
  - If `account` is present, it must be an object with a non-empty string
    `id` and a non-empty string `displayName`. Any violation (non-object,
    missing field, empty string, or an accessor that throws) produces a
    provider-level error output with the default account.
  - Provider-level error snapshots (runtime failures, script errors,
    validation failures) always carry the default account.
  - The returned `account` value is preserved unchanged through the runtime
    output, daemon cache, and serialized API responses.

  In discovery mode, host identity is authoritative and account-probe
  errors retain the discovered account (see below).

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
        { id: "work", displayName: "Work Account" },
        { id: "personal", displayName: "Personal Account" }
      ];
    }
  }
  ```

  **Capability detection:** `discoverAccounts` is detected only after the
  plugin script and any override script are evaluated. If absent, `null`,
  or `undefined`, the runtime uses legacy single-probe mode with the
  default account.

  If the `discoverAccounts` property accessor throws an exception (e.g. a
  Proxy trap), the runtime produces a single provider-level error output
  with the default account — it does not fall back to legacy mode.

  **Account context:** Each discovered account receives a child context
  that inherits from the base `__openusage_ctx`. The `ctx.account` object
  has immutable `id` and `displayName` fields matching the discovered
  descriptor. In legacy mode, the probe receives a child context with
  `ctx.account = { id: "default", displayName: "default" }` whose identity
  fields are similarly immutable.

  **Validation:** The returned array must contain 0–32 items. Each item
  must be an object with a non-empty string `id` (unique within the array)
  and a non-empty string `displayName`. Violations (non-array, missing or
  empty fields, duplicate ids, over 32 items, or an exception) produce a
  single provider-level error output with the default account.

  **Empty list:** A valid empty array produces zero outputs for that
  provider, clearing any previously cached snapshots.

  **Host-authoritative identity:** In discovery mode, if the probe result
  omits `account`, sets it to `null`/`undefined`, or returns an explicit
  `{id:"default",displayName:"default"}`, the runtime assigns the
  discovered account. If the probe returns an explicit `account` that
  differs in either `id` or `displayName`, the runtime produces an
  account-specific error carrying the discovered identity.

  **Failure isolation:** A per-account probe failure produces an error
  snapshot carrying that account's identity. Other accounts are unaffected.

  **Subscriptions:** All `subscribeFile` calls during `discoverAccounts`
  and each account probe are collected into a single provider-level set.
  File-monitor commands are sent once per provider with the unioned
  subscriptions.

  **Refresh atomicity:** Each provider refresh atomically replaces that
  provider's entire cache vector. A valid empty discovery list clears the
  provider's cached snapshots.

  > No bundled plugin has adopted `discoverAccounts` yet. This is a
  > forward-looking capability for multi-account provider support.

  Future multi-account providers will add additional items with the same
  `providerId` and a distinct account `id`. The collection remains a flat
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
