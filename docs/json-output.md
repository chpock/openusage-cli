# JSON output reference

This is the first-party reference for JSON returned by `openusage-cli` to CLI and
HTTP consumers. It describes serialized output, not JavaScript probe input or the
internal `PluginOutput` structure. For commands and request options, see
[Usage](usage.md); for configuration selection, see [Configuration](configuration.md).

## Contents

- [Contract and availability](#contract-and-availability)
- [Reading conventions](#reading-conventions)
- [Entry points and CLI state](#entry-points-and-top-level-shapes)
- [Usage snapshots and accounts](#usage-snapshots)
- [Metric lines and units](#metric-lines)
- [Errors and HTTP status](#errors)
- [Plugin metadata](#plugin-metadata)
- [Runtime configuration](#runtime-configuration)
- [Health and control](#health-and-control-acknowledgements)
- [Consumer compatibility](#consumer-compatibility-and-maintenance)

## Contract and availability

The formats below are implemented in the Rust runtime. Support for an output
shape does not establish that every provider can produce it: native data sources,
authentication and provider compatibility require separate validation. Integration
and compatibility status are recorded in the
[CrossUsage v1.6.1 report](crossusage-updates/v1.6.1.md).

| Feature | Availability |
| --- | --- |
| Usage snapshots, account identity, `text`, `progress`, `badge` | Implemented |
| Progress formats `percent`, `dollars`, `count` | Implemented |
| CLI state wrapper, plugin metadata, configuration, health, HTTP error/control responses | Implemented |
| Per-account `warning` | Implemented |
| `text.modelBreakdown`, `statusDot`, `expiryTooltip`, `resetCreditExpiries` | Implemented |
| `progress.sessionStartSignal` | Implemented |
| `barChart` | Implemented |
| Progress format `currency` | Implemented |

Examples use illustrative values, account names, metrics and paths. They are not
a promise that a particular provider emits those metrics or that every package
includes every plugin. JSON examples are complete values; tables identify when a
value is only a line or another nested object.

## Reading conventions

- Property names are case-sensitive and use **camelCase**. Enum values such as
  `barChart` and `missingResetDate` are case-sensitive too.
- An **always** property is present on every object of that kind. A nullable
  property is present but may be JSON `null`. An **optional** property is omitted
  when unavailable. An omitted value is not the same as numeric zero or `false`.
- Existing nullable properties retain their `null` representation. New optional
  extension properties are omitted when absent; they do not add `null` properties
  to existing responses.
- Strings are text, not HTML, shell commands or stable error codes unless a
  section explicitly defines codes. Treat provider-supplied text as untrusted.
- Numbers are JSON numbers, not strings. Fractional values are allowed unless a
  field is described as an integer. Output does not use `NaN` or infinity.
  Floating-point monetary values are not an exact decimal accounting format.
- JSON object member order, indentation and whitespace are not part of the
  contract. Do not parse output with line splitting or regular expressions.
- Identity comes from IDs, not array positions or human-readable labels. Line
  arrays retain metric order; charts retain point order. Not every provider
  returns every declared line on every refresh.
- Timestamps describe instants, not display-local time. `fetchedAt` uses RFC 3339
  in UTC; `resetsAt` may retain an explicit numeric timezone offset. Accept
   fractional seconds. Reset-credit expiry timestamps require timezone-qualified
   RFC3339 strings.
  Compare parsed instants, not strings, and do not assume a fixed string length.

## Entry points and top-level shapes

| Entry point | Successful JSON value |
| --- | --- |
| `query` or `query --type=usage` | Array of usage snapshots |
| `query --type=plugins` | Array of plugin metadata objects |
| `query --type=config` | One runtime configuration object |
| Any query with `--with-state` | Object containing `state` and `data`; `data` keeps the selected query's shape |
| `GET /v1/usage` | Array of usage snapshots |
| `GET /v1/usage/{provider}` | One usage snapshot, or no body with HTTP 204 |
| `POST /v1/probe` | Array of refreshed usage snapshots |
| `GET /v1/plugins` | Array of plugin metadata objects |
| `GET /v1/config` | One runtime configuration object |
| `GET /health` | One health object |
| `POST /v1/shutdown`, `POST /v1/restart` | One control acknowledgement object |

These are ordinary JSON values, not JSON Lines or a stream of incremental
updates. Arrays are not enclosed in a pagination object and have no `total`
property. An empty successful collection is `[]`, never `null`.

### CLI transport and state wrapper

A successful query writes one JSON value to stdout followed by a newline. Local
responses are compact; a daemon response may be forwarded with its own
whitespace. Diagnostics use stderr. Keep the streams separate when consuming
JSON.

Without `--with-state`, there is no wrapper. With it, the wrapper is:

```json
{
  "state": { "queryMode": "cache" },
  "data": []
}
```

| Property | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `state` | object | Always | CLI query execution metadata |
| `state.queryMode` | string | Always | `cache` or `direct` |
| `data` | array or object | Always | The unchanged usage, plugins or config payload |

`cache` means the CLI used a daemon HTTP response. For usage queries, the CLI
reads the daemon's cache without forcing a refresh. It does **not** certify the
age, success or completeness of each snapshot. For metadata/config queries it
only identifies the daemon path, not a provider data cache.

`direct` means local response generation, either explicitly requested with
`--use-daemon=false` or used as a fallback. Local usage queries attempt provider
refresh; local plugins/config queries do not probe providers. The flag does not
change the payload schema.

For example, the wrapped config payload is an object, not an array; a complete
example appears under [Runtime configuration](#runtime-configuration).

The wrapper is CLI-only. HTTP responses never acquire `state` or `data` from
this flag. A daemon-backed query describes that daemon's enabled plugins,
configuration and source selection, not a reconfiguration using the query's
runtime flags. Daemon responses are forwarded, not upgraded to the CLI binary's
schema. With different binary versions, consult the daemon's `appVersion` from
health/config; validating the outer array/object shape is not capability
negotiation.

A command-level failure exits unsuccessfully and reports diagnostics on stderr;
it does not promise a JSON error object on stdout. Conversely, provider failures
inside otherwise successful queries are represented in snapshots and need not
cause a nonzero exit status. A successful local query is not proof every initial
probe succeeded; an initialization-time refresh failure can leave the result
empty. `help`, `version`, `show-default-config`, daemon startup and service
installation are not covered by the query JSON contract; their output is text
or, for `show-default-config`, YAML.

## Usage snapshots

Each item represents **one account of one provider**. The array remains flat
when a provider has multiple accounts; there is no nested `accounts` object.

Example of a complete, currently supported collection:

```json
[
  {
    "providerId": "codex",
    "displayName": "Codex",
    "plan": "Pro",
    "lines": [
      {
        "type": "progress",
        "label": "Session",
        "used": 25.5,
        "limit": 100.0,
        "format": { "kind": "percent" },
        "resetsAt": "2026-10-24T17:00:00Z",
        "periodDurationMs": 18000000,
        "color": null
      }
    ],
    "fetchedAt": "2026-10-24T12:00:00Z",
    "account": {
      "id": "default",
      "origin": "native",
      "isActive": true
    }
  }
]
```

### Snapshot fields

| Property | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `providerId` | string | Always | Manifest plugin ID; used in provider filters and metadata lookup |
| `displayName` | string | Always | Provider display name, not an account name or stable identifier |
| `plan` | string or null | Always | Provider-defined plan description; `null` when unavailable, including many error results |
| `lines` | array of metric lines | Always | Measurements, display text and/or error badges; see [Metric lines](#metric-lines) |
| `fetchedAt` | timestamp string | Always | Host time when the probe output became a snapshot |
| `account` | account object | Always | Host-assigned account identity |
| `warning` | string | Optional | Human-readable qualification of this account's result, for example estimated or incomplete data |

Usage snapshots have **no `iconUrl` property**. Obtain icons and other static
provider information from plugin metadata, joining `providerId` to metadata `id`.
The internal plugin-engine result includes an icon, but it is not the public
snapshot DTO.

`plan` is opaque display text, not an object containing reset dates or quotas.
Use a progress line's `resetsAt` for its reset time. Labels, display names, plan
text and which metrics are available may change with provider/plugin updates.
The output has no stable machine-readable metric ID beyond its line type and
provider-specific label convention.

`fetchedAt` is not the provider's billing timestamp, the end of a usage period,
or a guarantee of a successful authenticated request. Error snapshots also have
it. Re-reading a cached snapshot does not advance it. A refresh can create a new
snapshot from a plugin/backend's own cached data; its newer `fetchedAt` does not
prove a new remote request or log-file parse. Different accounts/providers may
have different timestamps; the collection is not a globally simultaneous sample.

**Warning:** a warning supplements `lines`; it does not replace
identity, imply an HTTP error or automatically discard valid metrics. Its
absence is not proof all possible metrics are available. A warning-only probe
with no valid lines does not bypass the existing empty-result error behavior.

Complete snapshot with a warning:

```json
{
  "providerId": "example-provider",
  "displayName": "Example Provider",
  "plan": null,
  "warning": "Usage is estimated from local logs; remote totals are unavailable",
  "lines": [
    { "type": "text", "label": "Today", "value": "1.2K tokens", "color": null, "subtitle": null }
  ],
  "fetchedAt": "2026-10-24T12:00:00Z",
  "account": { "id": "default", "origin": "native", "isActive": true }
}
```

### Account identity and selection

| Property | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `account.id` | string | Always | Opaque, provider-scoped output account ID |
| `account.origin` | string | Always | Credential-source identifier; currently `native`, `opencode` or `pi` for enabled sources |
| `account.isActive` | boolean | Always | Activity annotation supplied by discovery, defaulting to `true`; not a success or freshness flag |
| `account.name` | string | Optional | Human-readable account label, when discovery provides one |

Use **`(providerId, account.id)`** as the snapshot key. The same account ID may
appear under different providers. Do not parse an ID, rely on its array index,
use `name` as identity, or assume that `origin` is encoded in the ID. Account
names need not be unique. `isActive: false` is not an instruction to the host to
skip a probe; source selection is separate from this annotation.

Single-account providers use `id: "default"`, `origin: "native"` and
`isActive: true`. Discovery can add accounts such as `opencode-0` and `pi-0`.
The bundled Codex, Copilot, Claude and OpenRouter overrides label Pi accounts with
`origin: "pi"` and `name: "Pi"`. There is no separate public `source` field. When descriptor
IDs collide, the host can emit deterministic IDs prefixed `acc_v1_` to preserve
uniqueness. Clients must treat both original and generated IDs as opaque; an ID
is not guaranteed unchanged after the credential identity or discovery topology
changes. The internal probe's account ID need not equal the normalized output ID.

`origin` is assigned by the host from discovery; it is not inferred from a path
and cannot be replaced by a probe's returned `account` object. An upstream plugin
that reads an OpenCode file itself can still have origin `native`. See
[Authentication sources](configuration.md#authentication-sources) for what source
selection does and does not restrict. Discovery-only details such as
`errorPolicy`, `stableSubjectKey`, `sourceRef` and `originNamespace` are not public
account fields.

A provider may have zero, one or many snapshots. Disabled sources, empty
discovery, policy-suppressed errors or a cache not yet populated can all result
in an absent account. Absence is not a zero measurement. Refresh replaces each
selected provider's complete account vector, including clearing it when there
are no outputs; do not keep removed account rows indefinitely on the client.
Discovery validates at most 32 descriptors before source filtering.

Example of two accounts in one collection:

```json
[
  {
    "providerId": "copilot",
    "displayName": "GitHub Copilot",
    "plan": "Pro",
    "lines": [
      { "type": "text", "label": "Status", "value": "Available", "color": null, "subtitle": null }
    ],
    "fetchedAt": "2026-10-24T12:00:00Z",
    "account": { "id": "default", "origin": "native", "isActive": true }
  },
  {
    "providerId": "copilot",
    "displayName": "GitHub Copilot",
    "plan": "Business",
    "lines": [
      { "type": "text", "label": "Status", "value": "Available", "color": null, "subtitle": null }
    ],
    "fetchedAt": "2026-10-24T12:00:01Z",
    "account": { "id": "opencode-0", "origin": "opencode", "isActive": true, "name": "Work" }
  }
]
```

The collection API returns all selected accounts. The single-provider endpoint
selects the `default` account if it exists, otherwise the sole cached account.
If several non-default accounts exist and no default exists, it returns **204**;
it does not choose the first account. There is no public account selector yet.

### Cache, refresh and collection filters

- `GET /v1/usage` reads cached snapshots unless `refresh=true` is supplied.
  Reading does not probe providers or synthesize uncached placeholders.
- `GET /v1/usage?refresh=true` refreshes selected providers and returns their new
  snapshots, not the entire unfiltered cache.
- `pluginIds` is a comma-separated exact-ID filter, not a glob or account filter.
  Whitespace and empty tokens are removed. An absent or empty query filter means
  all enabled providers. Unknown IDs in a collection filter are ignored; a
  nonempty filter containing only unknown IDs yields `[]`, not HTTP 404.
- `POST /v1/probe` requires a JSON object body. `{}`, or a missing/null `pluginIds`
  property, selects all enabled providers. `{"pluginIds":[]}` selects none and
  returns `[]`; it is different from an empty GET query filter. Duplicate IDs do
  not cause duplicate probes. Nonselected provider caches are left unchanged.
- Filters can change collection order, and refresh and cached-read paths need not
  use the same provider order. Do not key or merge accounts by array position.
- A refresh can return error snapshots with HTTP 200. Operational failures of the
  HTTP refresh itself can instead return HTTP 500; see [Errors](#errors).

Example request, not an output payload:

```json
{ "pluginIds": ["codex", "copilot"] }
```

## Metric lines

Every line has a `type` discriminator and a string `label`. Types are `text`,
`progress`, `badge`, and `barChart`. Type-specific fields below
apply only to that type; an absent text-only field on a progress line is not
missing data from a text line.

`label` is provider-defined display text, not a globally unique metric ID.
`color` is a display hint, conventionally a hexadecimal CSS color such as
`#22c55e`; the host does not guarantee a validated palette. Neither a missing
color nor a particular color determines success. `subtitle` is supporting display
text. These strings do not introduce HTML rendering requirements.

### Text line

| Property | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `type` | string `text` | Always | Line discriminator |
| `label` | string | Always | Metric/display label |
| `value` | string | Always | Already formatted display value, not a JSON number |
| `color` | string or null | Always | Optional display color |
| `subtitle` | string or null | Always | Supporting display text |
| `modelBreakdown` | array of model rows | Optional | Structured token/cost detail |
| `statusDot` | string | Optional | Provider severity hint; known values are `normal`, `warning`, `critical` |
| `expiryTooltip` | string | Optional | Human-readable expiry description |
| `resetCreditExpiries` | array of timestamp strings | Optional | Expiration instants for available reset credits |

Currently supported line example:

```json
{
  "type": "text",
  "label": "Account",
  "value": "user@example.test",
  "color": null,
  "subtitle": "Personal subscription"
}
```

`value` can contain formatted money, token counts, email addresses or arbitrary
provider text. Do not derive authoritative numeric values or units by parsing
this string when structured fields are available. A line labelled `Account` is
still display text; it does not replace the snapshot's `account` identity.

**Model row:**

| Property | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `model` | string | Always | Provider/model name |
| `tokens` | nonnegative integer | Always | Token count for this row |
| `costUsd` | number | Optional | Cost in **US dollars**, when available; can be an estimate |
| `percent` | number | Always | Plugin-provided percentage share for the breakdown |

Percentages are not recomputed by the host and need not sum to exactly 100 after
rounding. Token rows are not necessarily provider billing totals. A missing
`costUsd` means unknown, not free usage. Token counters are integers; consumers
that need exact large values should use parsers that preserve integers beyond
JavaScript's exact-number range.

Model rows with a blank/missing model name are discarded. Missing, nonnumeric or
nonfinite token counts normalize to zero; negative counts clamp to zero and
fractions truncate. Missing, nonnumeric or nonfinite percentages normalize to
zero; finite percentages are preserved rather than recomputed. Nonfinite or
nonnumeric costs are omitted. These are input-normalization rules, not evidence
that a provider reported zero usage.

Text-line example with model details:

```json
{
  "type": "text",
  "label": "Today",
  "value": "$1.25",
  "color": null,
  "subtitle": "Estimated from local logs",
  "modelBreakdown": [
    { "model": "model-a", "tokens": 75000, "costUsd": 1.0, "percent": 75.0 },
    { "model": "model-b", "tokens": 25000, "costUsd": 0.25, "percent": 25.0 }
  ]
}
```

**Expiry information:** `resetCreditExpiries` describes expiring
reset credits, not the progress quota's next reset time. Multiple credits can
share the same timestamp; preserve duplicates. The list need not account for
credits whose expiry is unknown. The plugin supplies order and severity; no
universal severity thresholds or count inference are defined. Use the timestamps
for time calculations rather than parsing `expiryTooltip`. Cached `statusDot`
and tooltip text do not automatically change as time passes.

Expiry entries are trimmed and must parse as timezone-qualified RFC3339 instants.
Malformed, non-string, empty or timezone-less entries are dropped with a diagnostic
warning; no timezone is inferred. Valid offsets, order and duplicates are
preserved. If no valid entries remain, the optional field is omitted without
removing the text metric or other valid metadata. This differs from
`progress.resetsAt`, whose legacy normalization is described below.

```json
{
  "type": "text",
  "label": "Rate Limit Resets",
  "value": "2 available",
  "color": null,
  "subtitle": null,
  "statusDot": "warning",
  "expiryTooltip": "Two reset credits expire soon",
  "resetCreditExpiries": ["2026-10-25T00:00:00Z", "2026-10-25T00:00:00Z"]
}
```

### Progress line

| Property | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `type` | string `progress` | Always | Line discriminator |
| `label` | string | Always | Metric/display label |
| `used` | number | Always | Consumed amount; finite and nonnegative |
| `limit` | number | Always | Positive finite quota/budget |
| `format` | object | Always | Units and suggested presentation, defined below |
| `resetsAt` | timestamp string or null | Always | Next reset instant when available |
| `periodDurationMs` | positive integer or null | Always | Length of the usage window in milliseconds |
| `color` | string or null | Always | Optional display color |
| `sessionStartSignal` | string | Optional | Provider-specific hint for interpreting a zero-usage session |

`used` means consumed, not remaining. Remaining amount can be calculated as
`limit - used`; it can be negative because over-quota `used > limit` is allowed.
The host does not cap `used` at `limit`. A zero limit is not a valid progress
line; a provider without a usable quota may emit text instead.

A reset time can be in the past, for example in a cached snapshot. Its presence
does not prove a new quota was fetched. Invalid plugin reset dates are omitted
from the normalized value as `null`; an accepted ISO-like date without a timezone
is interpreted as UTC. `periodDurationMs` is a duration, not an epoch timestamp.
A duration does not supply a missing reset instant or prove linear consumption.

#### Progress formats and units

| `format` | Meaning of `used` and `limit` |
| --- | --- |
| `{"kind":"percent"}` | Percentage points; `limit` is exactly `100`, so `used: 25` means 25%, not 0.25 |
| `{"kind":"dollars"}` | USD major units; `used: 1.25` means USD 1.25, not 1.25 cents |
| `{"kind":"count","suffix":"requests"}` | Quantities expressed in the given nonempty display unit; counts may be fractional |
| `{"kind":"currency","currency":"CNY"}` | Major units of the named currency; `used: 12.5` means CNY 12.50 |

A count suffix is provider-defined, for example `requests` or `tokens`. It is
not an account selector or machine-readable currency identifier. Use numeric
fields as numbers; no monetary format multiplies them by 100 on output.

A complete USD progress line:

```json
{
  "type": "progress",
  "label": "Monthly spend",
  "used": 12.34,
  "limit": 100.0,
  "format": { "kind": "dollars" },
  "resetsAt": "2026-11-01T00:00:00Z",
  "periodDurationMs": null,
  "color": null
}
```

A complete count progress line:

```json
{
  "type": "progress",
  "label": "Requests",
  "used": 42,
  "limit": 100,
  "format": { "kind": "count", "suffix": "requests" },
  "resetsAt": null,
  "periodDurationMs": null,
  "color": "#22c55e"
}
```

**Currency:** `currency` is a three-letter uppercase ASCII identifier in the
shape of an ISO 4217 currency code, not a symbol or exchange rate. The host
validates this shape, not membership in an assigned-code registry; for example,
`CNY` and `XYZ` pass, while `cny`, `CN` and `$` do not. Invalid codes produce an
error metric instead of being silently corrected. `dollars` remains USD; it is
not reinterpreted for non-USD providers. A currency line does not trigger conversion to USD or any
other currency. Never sum monetary lines in different currencies without an
explicit conversion outside this output contract. Display rounding does not
change the units of the numeric fields.

```json
{
  "type": "progress",
  "label": "Account spend",
  "used": 12.5,
  "limit": 100.0,
  "format": { "kind": "currency", "currency": "CNY" },
  "resetsAt": null,
  "periodDurationMs": null,
  "color": null
}
```

**Session hint:** known values are `missingResetDate` and `zeroUsage`.
For `missingResetDate`, an absent reset date identifies a not-yet-started window;
a reported reset date can mean the window has started even if reported usage
rounds to zero. `zeroUsage` identifies the usage-based interpretation of an idle
session. The hint is not a globally valid session state or a substitute for
`resetsAt`. Consumers that do not display session state can ignore it; consumers
that do should tolerate new hint strings.

```json
{
  "type": "progress",
  "label": "Session",
  "used": 0,
  "limit": 100,
  "format": { "kind": "percent" },
  "resetsAt": "2026-10-24T17:00:00Z",
  "periodDurationMs": 18000000,
  "color": null,
  "sessionStartSignal": "missingResetDate"
}
```

### Badge line

| Property | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `type` | string `badge` | Always | Line discriminator |
| `label` | string | Always | Badge/display label |
| `text` | string | Always | Badge text, not the `value` property used by text lines |
| `color` | string or null | Always | Optional display color |
| `subtitle` | string or null | Always | Supporting display text |

```json
{
  "type": "badge",
  "label": "Status",
  "text": "Connected",
  "color": "#22c55e",
  "subtitle": null
}
```

Badges are not inherently errors. The host's error representation is the reserved
badge label `Error`, described under [Errors](#errors).

### Bar chart line

A chart line is a small ordered numeric series. It supplies data for a consumer;
`openusage-cli` does not create a graphical interface or render the chart.

| Property | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `type` | string `barChart` | Always | Line discriminator |
| `label` | string | Always | Series/display label |
| `points` | array of points | Always | Ordered nonempty series of valid points |
| `note` | string | Optional | Context or estimation note |
| `color` | string | Optional | Optional display color for this new line type |

| Point property | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `label` | string | Always | Nonempty display label, not necessarily a parseable date |
| `value` | number | Always | Finite nonnegative measurement |
| `valueLabel` | string | Optional | Formatted display value, potentially including units |

```json
{
  "type": "barChart",
  "label": "Usage Trend",
  "points": [
    { "label": "Oct 23", "value": 1200, "valueLabel": "1.2K tokens" },
    { "label": "Oct 24", "value": 2400, "valueLabel": "2.4K tokens" }
  ],
  "note": "Estimated from local logs"
}
```

The type does not imply that values are USD, tokens, UTC daily buckets or any
particular time span. Preserve point order and duplicate labels. Do not invent
timestamps, a currency or missing points from human-readable labels. This is
not an API for fetching stored historical snapshots. Normalization examines at
most the first 366 points, matching the target upstream bound; later points are
not emitted. Invalid points produce error badges while valid points can remain
in the chart. If no valid points remain, the result is an error, not an empty
successful chart.

## Errors

### Provider errors inside successful JSON

A provider/account failure normally becomes a usage snapshot with a badge:

```json
{
  "providerId": "codex",
  "displayName": "Codex",
  "plan": null,
  "lines": [
    {
      "type": "badge",
      "label": "Error",
      "text": "Authentication is unavailable",
      "color": "#ef4444",
      "subtitle": null
    }
  ],
  "fetchedAt": "2026-10-24T12:00:00Z",
  "account": { "id": "default", "origin": "native", "isActive": true }
}
```

Detect a host-reported provider error by a line with `type: "badge"` and
`label: "Error"`, not by color, `plan: null`, missing metrics or HTTP status.
`text` is a human-readable message; wording is not a stable code. There is no
snapshot-level `error`, `success` or `status` field.

Normalizing a result can preserve valid lines alongside error badges for invalid
lines. A missing/empty `lines` result is converted into an error, not a successful
empty measurement list. Missing authentication, thrown exceptions and unsupported
metric types can also produce errors. An error snapshot replaces the previous
snapshot for that account; there is no separate last-successful-data fallback in
this response format.

An account-specific error retains that account identity. A provider-level
initialization/discovery failure uses the default account even if that account's
credential source would otherwise be filtered out. Other accounts/providers can
still succeed. A snapshot's presence, HTTP 200, or a successful CLI exit does
not by itself certify provider success.

Discovery may suppress a failed account under its private
`hide-if-other-account` policy when another enabled account descriptor exists,
even if the other account also fails. OpenCode- and Pi-origin errors remain visible.
Suppressed results are absent, not serialized with a hidden flag. See
[Discovery error policy](plugin-overrides.md#discovery-error-policy).

### HTTP status and error bodies

Application error responses have this shape:

```json
{ "error": "provider_not_found" }
```

| Status | Where | Body / meaning |
| --- | --- | --- |
| 200 | Successful read/refresh/control request | JSON value listed in the entry-point table; provider errors may be inside snapshots |
| 204 | Single-provider usage with no selected cached account | **No body**, not JSON `null` or `[]` |
| 404 | `/v1/usage/{provider}` for an unknown or disabled provider | `{"error":"provider_not_found"}` |
| 403 | Shutdown/restart from non-loopback remote | `shutdown_forbidden_remote` or `restart_forbidden_remote` in `error` |
| 403 | Shutdown/restart with a disallowed `Origin` | `shutdown_forbidden_origin` or `restart_forbidden_origin` in `error` |
| 503 | Shutdown/restart already triggered or unavailable | `shutdown_already_triggered` or `restart_already_triggered` in `error` |
| 500 | Internal HTTP operation failure | `error` contains diagnostic text, not a stable enum value |

A known provider can return 204 because no snapshot exists **or** because several
non-default accounts make single-account selection ambiguous. Use the collection
to distinguish these cases. Do not parse a 204 body as JSON.

Successful JSON and application JSON errors use `Content-Type: application/json`.
This is not a promise that every HTTP failure is JSON: malformed query/JSON
input, wrong content type, wrong methods and unknown routes can be rejected by
the HTTP framework with a text or empty body. Check status and content type
before decoding. Such rejections, proxy/network failures and connection refusal
are not provider error snapshots. Request-body validation and network errors do
not have a uniform machine-readable error-code vocabulary.

## Plugin metadata

`GET /v1/plugins` and `query --type=plugins` return an array of metadata for
successfully loaded, **enabled** plugins, including ones with no cached snapshots.
Discovered but disabled IDs appear in configuration's `availablePlugins.inactive`,
not this array. This is host metadata, not a verbatim `plugin.json` file.

```json
[
  {
    "id": "example-provider",
    "name": "Example Provider",
    "iconUrl": "data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciLz4=",
    "brandColor": null,
    "lines": [
      { "type": "progress", "label": "Monthly spend", "scope": "overview" },
      { "type": "text", "label": "Account", "scope": "detail" }
    ],
    "links": [{ "label": "Dashboard", "url": "https://example.test/dashboard" }],
    "primaryCandidates": ["Monthly spend"]
  }
]
```

| Property | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `id` | string | Always | Manifest ID; matches usage `providerId` |
| `name` | string | Always | Provider display name |
| `iconUrl` | string | Always | Base64 data URL containing the icon; not a filesystem path or an HTTP route |
| `brandColor` | string or null | Always | Provider brand display hint |
| `lines` | array of declarations | Always | Declared metric shapes; not measured usage values |
| `links` | array of link objects | Always | Sanitized useful links; empty array if none |
| `primaryCandidates` | array of strings | Always | Progress labels ordered by primary-metric preference; can be empty |

Each line declaration always has string fields `type`, `label` and `scope`.
Expected scopes are `overview` and `detail`; these describe upstream presentation
intent, not different HTTP permissions. `barChart` declarations accompany the
new chart output type. Declarations need not match the actual lines one-for-one:
a provider can omit unavailable detail and the host can add an error badge.
Primary candidates are labels, not indices, and need not have a usable measurement
in a particular snapshot. They come from progress declarations with
`primaryOrder`, ordered from lower to higher order values.

Each link has `label` and `url` strings. The host trims them, removes empty
label/URL entries, and retains only `http://` or `https://` URLs. No network request
or authorization check is implied by a link being present.

Consumers should use the MIME type in `iconUrl`, not assume all icons are SVG.
The current loader still labels all icons as SVG; correct PNG MIME handling is
pending in the compatibility update. Upstream manifest version, entry path,
raw `icon` filename, `primaryOrder` and JavaScript account/discovery internals
are not serialized metadata fields.

## Runtime configuration

`GET /v1/config` and `query --type=config` return effective runtime information,
not the YAML configuration file and not provider credentials. Values reflect
resolved CLI/config/default selection and discovered plugin availability.

```json
{
  "appVersion": "0.0.0",
  "host": "127.0.0.1",
  "port": 6738,
  "serviceMode": "standalone",
  "existingInstancePolicy": "error",
  "pluginsDir": "/usr/share/openusage-cli/plugins",
  "enabledPlugins": ["codex", "copilot"],
  "enabledAuthSources": { "default": ["native", "opencode", "pi"], "codex": ["opencode"] },
  "availablePlugins": { "active": ["codex", "copilot"], "inactive": ["claude"] },
  "appDataDir": "/home/example/.local/share/openusage-cli",
  "pluginOverridesDir": "/usr/share/openusage-cli/plugin-overrides",
  "refreshIntervalSecs": 180,
  "aggressiveRefreshIntervalSecs": 10,
  "logLevel": "error"
}
```

| Property | Type | Presence | Meaning |
| --- | --- | --- | --- |
| `appVersion` | string | Always | Version of the runtime supplying this response |
| `host` | string | Always | Configured bind host; a direct query does not bind an HTTP server |
| `port` | integer | Always | Daemon's bound port; in direct mode a configured value that can be `0` |
| `serviceMode` | string | Always | `standalone` or `systemd` |
| `existingInstancePolicy` | string | Always | `error`, `ignore` or `replace` |
| `pluginsDir` | string or null | Always | Resolved plugin directory; normally populated |
| `enabledPlugins` | array of strings | Always | Enabled-plugin selection masks, not necessarily expanded IDs |
| `enabledAuthSources` | object mapping to arrays of strings | Always | `default` and optional exact plugin-ID policies; source values currently `native`, `opencode`, `pi` |
| `availablePlugins` | object | Always | Active and inactive successfully discovered plugin IDs |
| `availablePlugins.active` | array of strings | Always | Enabled IDs, regardless of account visibility or probe success |
| `availablePlugins.inactive` | array of strings | Always | Discovered but disabled IDs |
| `appDataDir` | string or null | Always | Resolved application data/cache directory; normally populated |
| `pluginOverridesDir` | string or null | Always | Resolved overrides directory, or `null` if unavailable |
| `refreshIntervalSecs` | nonnegative integer | Always | Configured periodic refresh interval, in seconds |
| `aggressiveRefreshIntervalSecs` | nonnegative integer | Always | Configured aggressive refresh interval, in seconds |
| `logLevel` | string | Always | Runtime log filter setting; normally `error`, `warn`, `info`, `debug` or `trace` |

Path strings identify server-side paths and can reveal usernames. Do not assume
the client can access them, or reconstruct them using a client-side home directory.
The output does not include proxy URLs/credentials, the `use_daemon` preference,
`foreground`, or every original configuration-file property.

`enabledAuthSources.default` includes the built-in default when omitted from
input. A per-plugin list replaces that default; `[]` disables that plugin's
account probes, not its metadata. It is not a source-priority list. The active
list is different from the masks in `enabledPlugins`, which can contain globs
such as `*`. See [Authentication sources](configuration.md#authentication-sources)
for selection details.

For a daemon-backed CLI query, the response is the daemon's configuration. In
direct mode, host/port/service settings describe the effective query runtime,
not a listening server or successful daemon discovery.

Complete wrapped direct-config example:

```json
{
  "state": { "queryMode": "direct" },
  "data": {
    "appVersion": "0.0.0",
    "host": "127.0.0.1",
    "port": 0,
    "serviceMode": "standalone",
    "existingInstancePolicy": "error",
    "pluginsDir": "/usr/share/openusage-cli/plugins",
    "enabledPlugins": ["*"],
    "enabledAuthSources": { "default": ["native", "opencode", "pi"] },
    "availablePlugins": { "active": ["codex"], "inactive": [] },
    "appDataDir": "/home/example/.local/share/openusage-cli",
    "pluginOverridesDir": null,
    "refreshIntervalSecs": 180,
    "aggressiveRefreshIntervalSecs": 10,
    "logLevel": "error"
  }
}
```

## Health and control acknowledgements

`GET /health`:

```json
{ "status": "ok", "appVersion": "0.0.0", "pluginsLoaded": 2 }
```

All three fields are always present. `pluginsLoaded` is the nonnegative count of
enabled loaded plugins, not a count of accounts or successful probes. `status`
is the literal `ok` for this successful handler response; it is not an aggregate
provider health check or a promise of populated caches.

Successful `POST /v1/shutdown`:

```json
{ "status": "shutting_down" }
```

Successful `POST /v1/restart`:

```json
{ "status": "restarting" }
```

These acknowledgements mean a lifecycle command was accepted, not that shutdown
or restart has already finished. Failure bodies and loopback/origin restrictions
are described under [Errors](#errors) and [Usage](usage.md#api-endpoints).

## Consumer compatibility and maintenance

- Ignore unknown object properties when they do not affect the data you need.
  Treat absent optional extension fields as unavailable, not as zero or success.
- Handle unknown line types and format kinds explicitly: skip/report the
  unsupported metric, not the entire account or silently treat it as a known
  type. A client with a closed enum must be updated for `barChart` and `currency`.
- An additive type/field extension is not a guarantee that every existing strict
  client is compatible. No schema-version property or new HTTP API version is
  added by these extensions. Match your consumer to the documented availability
  and the runtime's `appVersion`.
- Preserve the distinction between `[]`, a missing account, a nullable property,
  an error badge, an optional warning and a bodyless HTTP 204.
- Do not aggregate overlapping account/provider measurements without knowing
  their meaning, period and units. Local estimates, API quotas, reset credits
  and actual invoices are not interchangeable.
- Update this reference alongside changes to serialization, cache DTOs, CLI
  envelopes, HTTP response bodies or units. Integration tests must cover the
  emitted JSON through the production pipeline, not just JavaScript builders.

The implementation anchors are the metric/account definitions in
[`src/plugin_engine/runtime.rs`](../src/plugin_engine/runtime.rs), public cache and
metadata DTOs in [`src/daemon.rs`](../src/daemon.rs), HTTP bodies in
[`src/http_api.rs`](../src/http_api.rs), CLI query/envelope serialization in
[`src/main.rs`](../src/main.rs), and authentication-source serialization in
[`src/config.rs`](../src/config.rs). Vendored JavaScript schemas alone do not
define this utility's public output.
