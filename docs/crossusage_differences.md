# CrossUsage vs openusage-cli: behavior differences

This file documents intentional behavior differences between the pinned CrossUsage plugin host and this repository (`openusage-cli`). The upstream revision is recorded in [crossusage-upstream.md](crossusage-upstream.md).

## Environment variable resolution

- **Upstream CrossUsage behavior**: `host.env.get(name)` first checks the process environment, then falls back to launching shell commands (for example `printenv` through login/interactive shell modes) to discover variables that are only present in user shell initialization.
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

- **Upstream CrossUsage behavior**: the plugin API does not include a file
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

- **Upstream CrossUsage behavior**: the Rust host can receive a configured
  provider-account context, expose its `instanceId`, `baseProviderId`, and
  `label`, and provide credential-store access through `host.credentials`.
  It does not use this project's JavaScript `discoverAccounts(ctx)` mechanism.
- **`openusage-cli` behavior**: extends the plugin API with an optional
  `discoverAccounts(ctx)` export. When present, the runtime calls it to
  obtain an array of account descriptors, then probes each account
  sequentially with an inherited context containing an immutable `account`
  identity. Override scripts can replace discovery via the declarative
  `__openusage_function_overrides` manifest (see
  [Plugin Overrides](plugin-overrides.md#declarative-function-overrides)).

Our plugin context deliberately exposes an immutable `account` containing only
`id`; source origin and selection are handled by the Rust core. Upstream's
configured account store is not interchangeable with our source discovery and
must not be introduced by silently reinterpreting these identifiers.

For detailed documentation, see [Usage](usage.md) (multi-account discovery
section) and [Plugin Overrides](plugin-overrides.md).

## Linux credential service

The Linux host uses freedesktop Secret Service through a local session-bus Unix
socket for ordinary plugin keychain reads and writes. It does not expose the
upstream GUI account store or change the account model described above.

- Service-only and explicit-username selectors remain independent. Reads prefer
  the first unlocked matching item; an exclusively locked match fails without
  calling Unlock or Prompt.
- A subsequent write through the same selector retains the selected object and
  its actual service/username. Removed or changed selected identities fail rather
  than silently selecting a replacement or creating another item. A deliberately
  different explicit selector is an independent operation.
- New-item creation checks the native response and rejects required prompts
  without displaying them. No default collection is silently created or unlocked.
- Current-user lookup uses process USER/USERNAME and native OS lookup, not shell
  commands or an invented fallback username.
- Only filesystem and Linux abstract Unix session sockets are supported. Other
  D-Bus transports fail explicitly before connector/helper I/O. Operations use
  an asynchronous deadline and a joined worker; no detached I/O worker is left
  running after return.
- Store-operation failures use fixed SDK errors; remote error descriptions are
  not included in application messages. Standard pre-credential D-Bus handshake
  diagnostics remain available. The filesystem sandbox keeps this bridge disabled.

Identity validation and SetSecret are separate protocol operations, not a
cross-process compare-and-set guarantee. Live desktop keyring interoperability
is distinct from controlled encrypted-wire validation. See the
[update report](crossusage-updates/v1.6.1.md) for verified coverage and limits.

Native callers must preserve their successful read selector during refresh. The
[Antigravity CLI correction](upstream-plugin-workarounds.md#cu-004-antigravity-cli-refresh-destination)
repairs that caller without weakening the generic selector contract.

## Chromium cookie sources

The Linux host supports native Chromium profile discovery, exact host/name
filters and legacy v10/v11 decryption. It uses the credential service above for
safe-storage keys; v12/v20 encryption and non-Linux decryption fail explicitly.
This helper does not provide Firefox support.

- `host.chromiumCookies.read` retains upstream's first-complete/most-complete
  single-profile selection. The additive `readProfiles` method returns separate
  nonempty maps in discovery order, without joining profiles or exposing profile
  metadata. Both accept `{hosts: string[], names: string[]}` with nonempty arrays
  and members; returned maps associate requested names with string values.
  The [Gemini Apps correction](upstream-plugin-workarounds.md#cu-005-gemini-apps-cookie-profile-selection)
  applies that caller's existing session predicate; it does not change the generic
  reader's definition of completeness.
- Source files are opened read-only and copied into private temporary directories
  (`0700`, files `0600`) before SQLite queries. Database and WAL copies are
  independent operations, **not** a transactional source snapshot. This preserves
  native behavior through exclusive browser locks without claiming consistency
  during concurrent writes.
- Failures use fixed diagnostics. Cleanup is attempted before return; cleanup
  failure is surfaced and can leave private material requiring removal. The
  filesystem sandbox denies both SDK methods before reader/keyring work.

## Local metrics and optional exporters

The host implements local Claude/Codex/Pi/Grok scanners, Cursor transcript
estimates, Antigravity SQLite accounting, authenticated Cursor export analytics
and optional installed-firectl billing export. These are current metric inputs,
not the upstream GUI account manager or historical usage database/API. Physical
log totals do not prove attribution to the selected OAuth account; transcript
estimates are not billed-token measurements.

Native backends retain source-specific cache rules and bind authenticated cache
entries to the effective source/login. Cursor defaults installation selection
from the actual manifest ID rather than requiring upstream GUI account fields.
Existing explicit helper options remain supported. Fireworks does not install
firectl; temporary credentials are private and finite helper jobs are reaped.
The [update report](crossusage-updates/v1.6.1.md) records the verified synthetic
paths and remaining interoperability limits. Browser/credential, SQLite and
external-helper sandbox denials are not evidence of positive backend behavior.

## Probe scope and diagnostics

The host remains probe-only. Upstream's GUI account store, historical usage
persistence, post-probe ingestion and state-changing plugin actions are not
exposed. There is no successful no-op API claiming those features exist.

Both hosts recreate JavaScript runtime state for each provider invocation;
module-local request throttling does not persist across refreshes. Our existing
per-operation timeouts remain in force rather than adopting upstream's aggregate
probe deadline. Helper-local budgets do not imply a total probe bound.

Diagnostic copies redact known cookie pairs, Devin tokens, plaintext email,
account paths/query parameters and identity fields in addition to the previous
secret patterns. Real HTTP payloads are unchanged. The established partial
redaction convention is retained; it is not a guarantee that every arbitrary
secret representation can be recognized. No live credentials were used to
validate these patterns.

## SQLite

Generic `host.sqlite.query` and `exec` use bundled rusqlite; no external sqlite3
executable is required. Query returns a JSON-array string from a read-only MAIN
connection. Normal opens see WAL data and can read/create sidecars. Only an open
failure attempts the pinned immutable fallback, which may ignore WAL content;
query/row errors do not trigger that fallback. Blobs become lossy UTF-8 strings,
nonfinite reals become JSON null, and invalid UTF-8 TEXT is an error. Exec opens
or creates the database and executes a batch, returning undefined. Its native
open flags allow URI handling; missing parents are not created, and failed SQL
can leave a newly created empty database.

The filesystem policy validates the selected path, not arbitrary SQL or sidecar
access. Plugins are trusted to issue SQL; this is not an ATTACH/VACUUM filesystem
sandbox. The public hermetic probe sandbox still disables both methods before
database access. Controlled binding/native-consumer fixtures do not certify
installed application schemas or successful platform-specific fallback recovery.
No aggregate SQL/probe deadline is introduced.

## ccusage compatibility

The host uses `ccusage@20.0.2` provider namespaces for Claude, Codex, Amp, Kimi,
Copilot and OpenCode (`opencode-go` aliases OpenCode). Unsupported providers do
not silently scan Claude. Only Claude/Codex retry legacy 18.0.11 commands after a
non-timeout failure; the runner priority remains bunx, pnpm, yarn, npm, then npx.

Candidate discovery checks `--version` through the same finite-job executor as
the query. An unavailable candidate does not hide another one; no verified
candidate produces `no_runner` before a package query. Availability timeout
continues discovery, while query timeout stops fallback. The existing 15 seconds
apply per invocation, not globally across candidates or an entire probe.

PATH enrichment follows upstream's literal `.nvm/alias/default` version-directory
expansion, not general NVM alias/selector resolution. Alias files must be regular,
valid UTF-8 and at most 4096 bytes; the trimmed token permits ASCII letters,
digits and `-._`. Missing, unreadable or invalid input adds no directory. Shell
profiles and NVM commands are not executed. These controlled command/SDK checks
do not establish package-manager, registry or live-provider interoperability.
