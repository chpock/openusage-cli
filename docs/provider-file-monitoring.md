# Provider file monitoring

`openusage-cli` extends the upstream plugin host API with reactive file-dependency
tracking.  Plugins can declare exact file paths they depend on; when a declared
file changes, the daemon automatically refreshes the affected providers.

## Purpose

The upstream OpenUsage plugin model is stateless: every probe runs from scratch
and does its own I/O.  Provider file monitoring does not reduce the number of
file reads per probe; rather, it provides reactive targeted refresh so that
when a credential or configuration file changes on disk, the daemon can update
its cached usage data without waiting for the next scheduled interval.

This is separate from the **daemon restart watcher**, which monitors plugins,
config, and binary files for the purpose of restarting the daemon process.  The
restart watcher is static (set at startup) and always triggers a full process
restart.  Provider file monitoring is dynamic (updated after every probe) and
triggers targeted cache refreshes without restarting the daemon.

## Plugin API: `ctx.host.fs.subscribeFile`

### Signature

```js
/**
 * Register an exact file dependency for the current probe.
 *
 * @param {string} path - File path (absolute, relative, or tilde-prefixed).
 * @returns {boolean} - `true` when the path is accepted and recorded,
 *                      `false` for empty, whitespace-only, or structurally
 *                      invalid paths.
 */
ctx.host.fs.subscribeFile(path) -> boolean
```

### Path resolution

- **Tilde expansion**: a leading `~/` or bare `~` is expanded to the user's home
  directory via `dirs::home_dir()`.  Other paths are stored as provided.
- **Lexical normalization**: when the monitor actor processes the subscription,
  it resolves relative paths against the daemon's current working directory to
  produce lexical absolute paths.  No symlink canonicalization is performed,
  keeping declared paths and filesystem-event paths symmetric.
- **Invalid paths**: empty strings, whitespace-only strings, and paths without a
  parent component are silently rejected (`false` is returned).

### Deduplication

The same path may be declared multiple times within a single probe.  Only the
first declaration is recorded; subsequent calls are no-ops.  This applies across
plugin entry script evaluation, override evaluation, and the `probe()` call.

### Declaration-time existence

Each subscription records whether the file existed (was `stat`-able) at the
moment `subscribeFile` was called.  After replacement is installed, the actor
compares the recorded existence with the current state.  If they differ, it
queues a debounced refresh.  This covers both an absent file created during the
gap and a present file removed during the gap.

### When to call

Every probe creates a fresh QuickJS runtime.  Subscriptions are collected per
probe and reset before the next probe.  Dependencies may be declared during:

- **Plugin entry script evaluation** (top-level code before `probe()` is called)
- **Override script evaluation** (top-level code in the override file)
- **The `probe()` function itself**

If a file does not exist yet but may be created later, declare it at
initialisation time so the monitor watches its parent directory for creation
events.

### Example

```js
// Override or plugin entry script — declares dependencies during evaluation.
(function () {
  try {
    var ctx = globalThis.__openusage_ctx;
    if (ctx && ctx.host && ctx.host.fs && typeof ctx.host.fs.subscribeFile === "function") {
      ctx.host.fs.subscribeFile("~/.config/app/tokens.json");
      ctx.host.fs.subscribeFile("~/.local/share/app/auth.json");
    }
  } catch (_) {}
})();

// Inside probe() — declares dependencies during probe execution.
function probe(ctx) {
  // The same path is a no-op if already declared above.
  ctx.host.fs.subscribeFile("~/.config/app/tokens.json");
  // ... read and return usage data
}
```

## Daemon lifecycle

### Probe loop

During `DaemonState::refresh`, each provider goes through this sequence:

1. **ClearProvider**: the daemon sends a `ClearProvider` command to the monitor
   actor, removing all previous file subscriptions for this provider.  The
   daemon awaits the acknowledgement before proceeding to the probe.
2. **Probe**: the plugin's `probe()` function runs.  Any `subscribeFile` calls
   during this phase are collected into a per-probe list.
3. **ReplaceProvider**: the daemon sends a `ReplaceProvider` command with the
   collected subscription list.  The monitor actor replaces the provider's
   subscriptions.  Individual file paths whose watch installation fails are
   silently skipped; the remaining paths are still registered.  The daemon
   awaits the acknowledgement.
4. **Cache**: the probe output is stored in the cache regardless of whether the
   monitor commands succeeded or failed.

### Error handling

If the monitor actor is unavailable (channel closed, or ack cancelled), the
daemon logs a warning and continues.  A failed or cancelled acknowledgement
never prevents probe execution or cache updates.  Individual watch registration
failures (for example OS-level watch limit exhaustion for a specific path) are
logged and that path is skipped, but other paths from the same probe are still
registered.

### Empty replacement

If a probe declares no files (no `subscribeFile` calls), the `ReplaceProvider`
command carries an empty file list.  This removes all previous subscriptions for
that provider, so the provider will no longer receive targeted refreshes from
file changes.

## Debounce and coalescing

The monitor actor uses a single global trailing debounce:

- **Deadline**: 1 second after the most recent filesystem event that touched a
  registered file.
- **Behaviour**: when the deadline fires, the actor unions all provider IDs that
  were affected by events during the window, deduplicates them, sorts them, and
  emits a single `ProvidersChanged` event.
- **Command priority**: incoming `ClearProvider`/`ReplaceProvider` commands are
  processed immediately, even during a pending debounce deadline. Clearing a
  provider removes it from pending snapshots; commands do not themselves emit a
  refresh unless the replacement race check observes an existence transition.

The dispatcher task in the daemon:

1. Awaits the first `ProvidersChanged` event.
2. Runs `daemon.refresh(Some(ids))` inline — no detached child tasks.
3. After the refresh completes, drains any queued events via `try_recv` and
   unions their IDs.
4. Runs at most one follow-up refresh with the drained IDs.
5. Loops back to waiting for the next event.

## Many-to-many sharing

Multiple providers can subscribe to the same file.  When that file changes, all
dependent providers are refreshed. When a provider stops declaring a file, its
subscription is removed without affecting other providers that still depend on
the same file. The OS-level watch is removed only when its last target file is
removed from the watch root.

## Watch strategy

### Non-recursive frontier

When the immediate parent exists, the monitor watches that directory, not the
file itself, and not recursively.  This means:

- **File creation**: detecting a new file at a declared path works because the
  parent directory is watched for `Create` events.
- **File deletion**: detecting a deleted file works because the parent directory
  is watched for `Remove` events.
- **File modification**: detecting a modified file works because the parent
  directory is watched for `Modify` events.
- **Atomic replacement** (rename over): detected as a `Create` event on the new
  inode (or `Modify` depending on the filesystem).

### Absent file or parent

If the file's parent directory does not exist at subscription time, the monitor
uses the nearest existing, non-root ancestor as a **frontier** watch root.  The
watch remains non-recursive.  When the next directory component appears, the
monitor migrates the watch inward.  It therefore detects a later file creation,
including a directory that arrives already containing the target file.

The monitor never watches the filesystem root and never installs a broad
recursive home-directory watch.  If no existing non-root ancestor is available,
the dependency cannot be watched until a later probe declares it again.

### Migration on directory changes

When a watched parent directory is deleted, renamed, or recreated, the monitor
invalidates its old OS watch and reconciles every target attached to that root.
It migrates outward or inward as needed and installs a fresh watch before later
file changes are trusted.  A deletion or recreation of the declared file causes
a debounced targeted refresh; an intermediate directory change only changes
watch placement.

### Root safety

Paths whose parent is the filesystem root are skipped.  The monitor never
installs a watch on `/`.

## Codex and Copilot behaviour

Both the Codex and Copilot overrides predeclare their two OpenCode candidate
paths during **override evaluation** (before `probe()` is called).  This is the
sole subscription registration for these files; the fallback functions that read
them do not independently call `subscribeFile`.

The predeclared paths are:

1. `~/.local/share/opencode/auth.json`
2. `~/.config/opencode/auth.json`

- **Primary auth available**: both candidate paths are declared but never read
  by the override.  The monitor watches them anyway; if they change, the
  provider is refreshed.
- **Fallback used**: the same two paths (already declared).  No duplicate
  subscriptions.
- **Invalid JSON or missing file**: the paths are still declared; the monitor
  watches their parent or frontier ancestor.  Changes to these files will
  trigger a refresh even if the current probe could not read them.

The auth priority, fallback order, and persistence behaviour are unchanged from
the upstream plugin contract.

## Symbolic link limitation

The monitor stores declared paths as lexical (non-canonicalised) paths.  If a
declared path is a symbolic link, the monitor watches the **link's parent
directory** for changes to the link entry itself.  External modifications to the
link's target (for example editing a file that the link points to without
touching the link) are **not** detected, because the watch is on the link's
parent, not the target's parent.

If the link is atomically replaced (the link entry itself is rewritten), the
parent directory fires a `Create`/`Modify` event and the change is detected.

## Operational notes

- The monitor actor is started before the initial plugin refresh, so the first
  probe's subscriptions are captured.
- When the monitor actor's OS-level watcher fails to initialise (for example
  `inotify` instance limit exhaustion), the daemon continues without dynamic
  provider monitoring.  All probes still execute and cache normally.
- Individual watch registration failures (for example a per-user `inotify` watch
  limit) cause that file path to be skipped, but other paths from the same probe
  are still registered.
- The dispatcher task is shut down before the monitor actor during graceful
  daemon shutdown, ensuring no in-flight refresh is interrupted by actor
  teardown.
- Subscription state is entirely in-memory.  After a daemon restart, all
  subscriptions are re-established by the next probe cycle.