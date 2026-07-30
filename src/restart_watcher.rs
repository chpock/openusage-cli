use anyhow::{Context, Result};
use notify::{
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher, event::ModifyKind,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const RESTART_DEBOUNCE_DELAY: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Public types (restart-monitor infrastructure, preserved)
// ---------------------------------------------------------------------------

/// Typed action emitted by the file monitor when a watched change is detected.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WatchAction {
    /// Restart the daemon process.
    Restart,
}

/// A registration describes what filesystem paths to watch and what action
/// to emit when a matching change is detected.
#[derive(Debug, Clone)]
pub struct WatchRegistration {
    pub action: WatchAction,
    pub debounce: Duration,
    pub description: String,
    pub(crate) dir_roots: Vec<PathBuf>,
    pub(crate) file_aliases: Vec<PathBuf>,
    pub(crate) watch_roots: Vec<WatchRoot>,
}

impl WatchRegistration {
    /// Create a new registration with the given action, debounce, and label.
    pub fn new(action: WatchAction, debounce: Duration, description: impl Into<String>) -> Self {
        Self {
            action,
            debounce,
            description: description.into(),
            dir_roots: Vec::new(),
            file_aliases: Vec::new(),
            watch_roots: Vec::new(),
        }
    }

    /// Add a directory root to watch (recursive or non-recursive).
    pub fn add_dir_root(&mut self, path: PathBuf, recursive: RecursiveMode) {
        add_watch_root(&mut self.watch_roots, path.clone(), recursive);
        self.dir_roots.push(path);
    }

    /// Add an exact file to watch (watches its parent directory).
    pub fn add_file(&mut self, path: &Path) -> Result<()> {
        let aliases = file_aliases(path);
        self.file_aliases.extend(aliases);
        add_file_watch(&mut self.watch_roots, path)
    }
}

#[derive(Debug, Clone)]
pub struct RestartWatchInputs {
    pub plugins_dir: PathBuf,
    pub plugin_overrides_dir: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub binary_file: PathBuf,
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct WatchRoot {
    pub path: PathBuf,
    pub recursive: RecursiveMode,
}

// ---------------------------------------------------------------------------
// Public API: spawn the generalized file monitor (restart infrastructure)
// ---------------------------------------------------------------------------

/// Spawn a file-monitor task that watches all roots from the given
/// registrations and emits [`WatchAction::Restart`] when changes are detected.
/// Multiple events within the debounce window reset the timer.
pub fn spawn_file_monitor(
    registrations: Vec<WatchRegistration>,
    action_tx: mpsc::UnboundedSender<WatchAction>,
) -> Result<tokio::task::JoinHandle<()>> {
    let (event_tx, event_rx) = mpsc::unbounded_channel::<notify::Result<Event>>();

    let mut watcher = RecommendedWatcher::new(
        move |event_result| {
            let _ = event_tx.send(event_result);
        },
        Config::default(),
    )
    .context("failed to initialize filesystem watcher backend")?;

    let total_roots: usize = registrations.iter().map(|r| r.watch_roots.len()).sum();

    for registration in &registrations {
        for watch_root in &registration.watch_roots {
            watcher
                .watch(&watch_root.path, watch_root.recursive)
                .with_context(|| {
                    format!(
                        "failed to watch path {} (recursive={:?}) for '{}'",
                        watch_root.path.display(),
                        watch_root.recursive,
                        registration.description,
                    )
                })?;
        }
    }

    log::info!(
        "filesystem watcher enabled (registrations={}, watch roots={})",
        registrations.len(),
        total_roots,
    );

    let watcher_task = tokio::spawn(async move {
        let _watcher = watcher;
        run_file_monitor_loop(registrations, event_rx, action_tx).await;
    });

    Ok(watcher_task)
}

// ---------------------------------------------------------------------------
// Registration builders
// ---------------------------------------------------------------------------

/// Build a restart registration from the traditional restart-watch inputs.
pub fn build_restart_registration(inputs: RestartWatchInputs) -> Result<WatchRegistration> {
    let cwd = std::env::current_dir().context("cannot resolve current directory")?;
    let mut reg = WatchRegistration::new(
        WatchAction::Restart,
        RESTART_DEBOUNCE_DELAY,
        "daemon restart (plugins, overrides, config, binary)",
    );

    let plugins_dir = normalize_existing_path(&inputs.plugins_dir, &cwd);
    if !plugins_dir.is_dir() {
        anyhow::bail!(
            "plugins directory is not readable: {}",
            plugins_dir.display()
        );
    }
    reg.add_dir_root(plugins_dir.clone(), RecursiveMode::Recursive);

    if let Some(overrides_dir_raw) = inputs.plugin_overrides_dir {
        let overrides_dir = normalize_existing_path(&overrides_dir_raw, &cwd);
        if !overrides_dir.is_dir() {
            anyhow::bail!(
                "plugin overrides directory is not readable: {}",
                overrides_dir.display()
            );
        }
        reg.add_dir_root(overrides_dir.clone(), RecursiveMode::Recursive);
    }

    if let Some(config_file_raw) = inputs.config_file {
        let config_file = normalize_absolute_path(&config_file_raw, &cwd);
        reg.add_file(&config_file)?;
    }

    let binary_file = normalize_absolute_path(&inputs.binary_file, &cwd);
    reg.add_file(&binary_file)?;

    Ok(reg)
}

// ---------------------------------------------------------------------------
// Event handling (generalized)
// ---------------------------------------------------------------------------

fn handle_event_general(
    event_result: notify::Result<Event>,
    registrations: &[WatchRegistration],
    restart_deadline: &mut Option<tokio::time::Instant>,
) {
    let event = match event_result {
        Ok(event) => event,
        Err(err) => {
            log::warn!("filesystem watcher backend error: {}", err);
            return;
        }
    };

    if !is_interesting_event_kind(&event.kind) {
        return;
    }

    let event_details = format_event_details(&event);
    let mut matched_any = false;

    for registration in registrations {
        let Some(change_description) = event
            .paths
            .iter()
            .find_map(|path| classify_path_for_registration(path, registration))
        else {
            continue;
        };

        matched_any = true;

        if restart_deadline.is_some() {
            log::info!(
                "detected additional filesystem change ({change_description}; {event_details}); restarting debounce timer ({}s)",
                registration.debounce.as_secs(),
            );
        } else {
            log::info!(
                "detected filesystem change ({change_description}; {event_details}); scheduling daemon restart in {}s",
                registration.debounce.as_secs(),
            );
        }
        *restart_deadline = Some(tokio::time::Instant::now() + registration.debounce);
    }

    if !matched_any {
        log::debug!(
            "ignored filesystem event (no matching registration): {}",
            event_details,
        );
    }
}

fn classify_path_for_registration(path: &Path, registration: &WatchRegistration) -> Option<String> {
    let normalized = normalize_event_path(path);

    // Check directory roots (prefix match).
    for dir_root in &registration.dir_roots {
        if normalized.starts_with(dir_root) {
            return Some(format!(
                "{}: {}",
                registration.description,
                normalized.display()
            ));
        }
    }

    // Check exact file aliases.
    if path_matches_aliases(&normalized, &registration.file_aliases) {
        return Some(format!(
            "{}: {}",
            registration.description,
            normalized.display()
        ));
    }

    None
}

// ---------------------------------------------------------------------------
// Dynamic provider file-monitor actor
// ---------------------------------------------------------------------------

/// Command sent to the provider watch actor.
///
/// The acknowledgement (`ack`) is sent once the command has been fully
/// processed by the actor's state machine.  It does **not** guarantee that
/// every individual filesystem watch operation succeeded; per-file watch
/// failures are logged and the file is skipped without crashing the actor.
#[derive(Debug)]
pub enum ProviderWatchCommand {
    /// Remove all file subscriptions for the given provider.
    ClearProvider {
        provider_id: String,
        ack: oneshot::Sender<()>,
    },
    /// Atomically replace file subscriptions for the given provider.
    /// An empty `files` list means no dependencies.
    ReplaceProvider {
        provider_id: String,
        files: Vec<FileSubscription>,
        ack: oneshot::Sender<()>,
    },
}

/// Event emitted by the provider watch actor when a debounced change fires.
#[derive(Debug, Clone)]
pub enum ProviderWatchEvent {
    /// One or more registered files changed; refresh these provider IDs.
    ProvidersChanged(Vec<String>),
}

// ---------------------------------------------------------------------------
// WatchBackend trait (fakeable for testing)
// ---------------------------------------------------------------------------

/// Abstracts the OS-level filesystem watch operations so the actor can be
/// tested without real `notify` timers.
pub(crate) trait WatchBackend: Send {
    fn watch(&mut self, path: &Path, recursive: RecursiveMode) -> notify::Result<()>;
    fn unwatch(&mut self, path: &Path) -> notify::Result<()>;
}

/// Real backend wrapping `notify::RecommendedWatcher`.
struct NotifyBackend(RecommendedWatcher);

impl WatchBackend for NotifyBackend {
    fn watch(&mut self, path: &Path, recursive: RecursiveMode) -> notify::Result<()> {
        self.0.watch(path, recursive)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.0.unwatch(path)
    }
}

/// Fake backend for testing the actor's state machine without real OS events.
#[cfg(test)]
pub(crate) struct FakeBackend {
    pub watched: HashMap<PathBuf, RecursiveMode>,
    pub fail_next_watch: Option<notify::Error>,
    pub fail_next_unwatch: Option<notify::Error>,
}

#[cfg(test)]
impl FakeBackend {
    pub fn new() -> Self {
        Self {
            watched: HashMap::new(),
            fail_next_watch: None,
            fail_next_unwatch: None,
        }
    }
}

#[cfg(test)]
impl WatchBackend for FakeBackend {
    fn watch(&mut self, path: &Path, recursive: RecursiveMode) -> notify::Result<()> {
        if let Some(err) = self.fail_next_watch.take() {
            return Err(err);
        }
        self.watched.insert(path.to_path_buf(), recursive);
        Ok(())
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        if let Some(err) = self.fail_next_unwatch.take() {
            return Err(err);
        }
        self.watched.remove(path);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal actor state
// ---------------------------------------------------------------------------

/// State for a single OS-level watch root (non-recursive directory watch).
struct WatchRootState {
    /// All target files currently anchored at this root.
    targets: HashSet<PathBuf>,
}

/// Per-target file state.
struct TargetState {
    /// Providers interested in this file.
    providers: HashSet<String>,
    /// Current watch root (directory being watched non-recursively).
    current_root: PathBuf,
    /// Last observed existence of the target file.
    last_observed_existence: bool,
    /// Existence at declaration time (for replacement race comparison).
    /// Set during ReplaceProvider, cleared after first reconciliation.
    declaration_existence: Option<bool>,
}

struct ProviderWatchActor {
    /// provider_id → set of watched file paths
    provider_files: HashMap<String, HashSet<PathBuf>>,
    /// file path → TargetState
    targets: HashMap<PathBuf, TargetState>,
    /// watch root directory → WatchRootState
    watch_roots: HashMap<PathBuf, WatchRootState>,
    /// Single global trailing debounce deadline.
    global_deadline: Option<tokio::time::Instant>,
    /// Provider snapshots captured at event time, keyed by file path.
    pending_snapshots: HashMap<PathBuf, HashSet<String>>,
    /// Backend for OS watching
    backend: Box<dyn WatchBackend>,
    /// Channel to emit events to the consumer
    event_tx: mpsc::UnboundedSender<ProviderWatchEvent>,
}

impl ProviderWatchActor {
    /// Install a non-recursive OS watch on `root` if not already installed.
    /// Returns true if the watch was newly installed.
    /// If the root is already in the registry but its directory no longer
    /// exists on disk, the stale entry is removed and a fresh watch is
    /// installed (the backend watch was invalidated when the directory was
    /// removed).  Attached targets are preserved through the reinstall.
    fn install_watch(&mut self, root: &Path) -> Result<bool> {
        if let Some(state) = self.watch_roots.get(root) {
            if root.exists() {
                // Root exists and is in the registry — backend watch is valid.
                return Ok(false);
            }
            // Root is in the registry but the directory no longer exists.
            // The backend watch was invalidated when the directory was
            // removed.  Reinstall the watch and preserve attached targets.
            let saved_targets = state.targets.clone();
            self.backend
                .watch(root, RecursiveMode::NonRecursive)
                .with_context(|| format!("cannot reinstall stale watch on '{}'", root.display()))?;
            self.watch_roots.insert(
                root.to_path_buf(),
                WatchRootState {
                    targets: saved_targets,
                },
            );
            log::debug!("reinstalled stale watch on '{}'", root.display());
            return Ok(true);
        }
        self.backend
            .watch(root, RecursiveMode::NonRecursive)
            .with_context(|| format!("cannot watch root '{}'", root.display()))?;
        self.watch_roots.insert(
            root.to_path_buf(),
            WatchRootState {
                targets: HashSet::new(),
            },
        );
        Ok(true)
    }

    /// Remove the OS watch on `root` if no targets remain.
    /// On unwatch failure, retains the installed state unless the root
    /// directory has been deleted — in that case the root is removed from
    /// the registry so a later recreated directory can be reinstalled.
    fn release_watch_if_empty(&mut self, root: &Path) {
        if let Some(state) = self.watch_roots.get(root)
            && !state.targets.is_empty()
        {
            return;
        }
        if self.watch_roots.contains_key(root) {
            if let Err(err) = self.backend.unwatch(root) {
                // If the root no longer exists on disk, the backend watch
                // is already dead — remove it so a later recreated directory
                // can be reinstalled as a fresh watch.
                if !root.exists() {
                    log::warn!(
                        "root '{}' was deleted; removing stale watch entry: {}",
                        root.display(),
                        err,
                    );
                    self.watch_roots.remove(root);
                } else {
                    log::warn!(
                        "failed to unwatch root '{}': {}; retaining installed watch root",
                        root.display(),
                        err,
                    );
                }
            } else {
                self.watch_roots.remove(root);
            }
        }
    }

    /// Compute the nearest existing non-root ancestor directory for `target`.
    /// Returns `None` if only the filesystem root exists.
    fn compute_root(&self, target: &Path) -> Option<PathBuf> {
        let parent = target.parent()?;
        nearest_non_root_ancestor(parent)
    }

    /// Reconcile a single target: recompute its root, migrate if needed,
    /// check existence, and enqueue providers if existence changed.
    /// Returns true if a debounced refresh was scheduled.
    fn reconcile_target(&mut self, target: &Path) -> bool {
        let Some(new_root) = self.compute_root(target) else {
            // No valid root — target is unreachable (e.g. only root exists).
            // Remove from old root if present.
            if let Some(state) = self.targets.get(target) {
                let old_root = state.current_root.clone();
                if let Some(root_state) = self.watch_roots.get_mut(&old_root) {
                    root_state.targets.remove(target);
                }
                self.release_watch_if_empty(&old_root);
                if let Some(state) = self.targets.get_mut(target) {
                    state.current_root = PathBuf::new(); // sentinel: no root
                }
            }
            return false;
        };

        let old_root = self.targets.get(target).map(|s| s.current_root.clone());

        // Install new root first (transactional: install before releasing old).
        if self.install_watch(&new_root).is_err() {
            // New root installation failed — keep old root if it exists.
            return false;
        }

        // Move target ownership to new root.
        self.watch_roots
            .entry(new_root.clone())
            .or_insert_with(|| WatchRootState {
                targets: HashSet::new(),
            })
            .targets
            .insert(target.to_path_buf());

        // Release old root.
        if let Some(old) = old_root
            && old != new_root
        {
            if let Some(root_state) = self.watch_roots.get_mut(&old) {
                root_state.targets.remove(target);
            }
            self.release_watch_if_empty(&old);
        }

        // Update target's current root.
        if let Some(state) = self.targets.get_mut(target) {
            state.current_root = new_root;
        }

        // Check existence change.
        let now_exists = target.exists();
        let mut changed = false;

        if let Some(state) = self.targets.get_mut(target) {
            // Check declaration-to-install race first.
            if let Some(declared) = state.declaration_existence.take()
                && now_exists != declared
            {
                changed = true;
            }

            if now_exists != state.last_observed_existence {
                changed = true;
                state.last_observed_existence = now_exists;
            }
        }

        if changed && let Some(providers) = self.targets.get(target) {
            self.pending_snapshots
                .entry(target.to_path_buf())
                .or_default()
                .extend(providers.providers.iter().cloned());
            self.global_deadline = Some(tokio::time::Instant::now() + RESTART_DEBOUNCE_DELAY);
        }

        changed
    }

    /// Reconcile all targets attached to a given root.
    fn reconcile_root(&mut self, root: &Path) {
        let affected: Vec<PathBuf> = self
            .watch_roots
            .get(root)
            .map(|state| state.targets.iter().cloned().collect())
            .unwrap_or_default();

        for target in affected {
            self.reconcile_target(&target);
        }
    }

    fn handle_replace_provider(&mut self, provider_id: &str, files: &[FileSubscription]) {
        // Clear existing subscriptions for this provider.
        self.clear_provider_internal(provider_id);

        if files.is_empty() {
            return;
        }

        let cwd = std::env::current_dir().ok();
        let mut added_files = HashSet::new();

        for sub in files {
            let normalized = normalize_file_path(&sub.path, cwd.as_deref());

            let _parent = match normalized.parent() {
                Some(p) => p.to_path_buf(),
                None => {
                    log::warn!(
                        "provider '{}' file '{}' has no parent; skipping",
                        provider_id,
                        normalized.display()
                    );
                    continue;
                }
            };

            // Compute root.
            let root = match self.compute_root(&normalized) {
                Some(r) => r,
                None => {
                    log::warn!(
                        "provider '{}' file '{}' has no valid non-root ancestor; skipping",
                        provider_id,
                        normalized.display()
                    );
                    continue;
                }
            };

            // Install watch on root.
            if let Err(err) = self.install_watch(&root) {
                log::warn!(
                    "provider '{}' cannot watch root '{}' for '{}': {}; skipping",
                    provider_id,
                    root.display(),
                    normalized.display(),
                    err,
                );
                continue;
            }

            // Register target.
            let now_exists = normalized.exists();
            let entry = self.targets.entry(normalized.clone()).or_insert_with(|| {
                let mut providers = HashSet::new();
                providers.insert(provider_id.to_string());
                TargetState {
                    providers,
                    current_root: root.clone(),
                    last_observed_existence: now_exists,
                    declaration_existence: Some(sub.existed_at_declaration),
                }
            });

            // If target already existed (shared by another provider), add this provider.
            entry.providers.insert(provider_id.to_string());
            // Update declaration_existence if this is a new observation.
            if entry.declaration_existence.is_none() {
                entry.declaration_existence = Some(sub.existed_at_declaration);
            }

            // Track in root.
            self.watch_roots
                .entry(root)
                .or_insert_with(|| WatchRootState {
                    targets: HashSet::new(),
                })
                .targets
                .insert(normalized.clone());

            // Track provider → files.
            added_files.insert(normalized);
        }

        // Register provider → files.
        if !added_files.is_empty() {
            self.provider_files
                .insert(provider_id.to_string(), added_files);
        }

        // Close declaration-to-install race for each newly added target.
        for file in self
            .provider_files
            .get(provider_id)
            .map(|f| f.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
        {
            self.reconcile_target(&file);
        }
    }

    fn handle_clear_provider(&mut self, provider_id: &str) {
        self.clear_provider_internal(provider_id);
    }

    fn clear_provider_internal(&mut self, provider_id: &str) {
        // Remove from pending snapshots.
        for snapshot in self.pending_snapshots.values_mut() {
            snapshot.remove(provider_id);
        }

        // Remove provider from all targets.
        if let Some(files) = self.provider_files.remove(provider_id) {
            for file in &files {
                if let Some(state) = self.targets.get_mut(file) {
                    state.providers.remove(provider_id);
                    if state.providers.is_empty() {
                        // Remove target entirely.
                        let root = state.current_root.clone();
                        self.targets.remove(file);
                        self.pending_snapshots.remove(file);
                        // Remove from root tracking.
                        if let Some(root_state) = self.watch_roots.get_mut(&root) {
                            root_state.targets.remove(file);
                        }
                        self.release_watch_if_empty(&root);
                    }
                }
            }
        }
    }

    fn handle_fs_event(&mut self, event: notify::Result<Event>) {
        let event = match event {
            Ok(e) => e,
            Err(err) => {
                log::warn!("provider watch backend error: {}", err);
                return;
            }
        };

        let is_remove = matches!(
            event.kind,
            EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
        );

        if !is_interesting_event_kind(&event.kind) {
            return;
        }

        for path in &event.paths {
            let normalized = normalize_event_path(path);

            // Direct file event: the event path matches a registered target.
            // Always enqueue providers regardless of existence change
            // (covers modification, creation, deletion of the target itself).
            if let Some(state) = self.targets.get(&normalized) {
                self.pending_snapshots
                    .entry(normalized.clone())
                    .or_default()
                    .extend(state.providers.iter().cloned());
                self.global_deadline = Some(tokio::time::Instant::now() + RESTART_DEBOUNCE_DELAY);
                // Also update last observed existence.
                if let Some(state) = self.targets.get_mut(&normalized) {
                    state.last_observed_existence = normalized.exists();
                }
                continue;
            }

            // Root event: the event's parent is a watch root, or the event
            // path itself is a watch root.  Reconcile all targets attached
            // to that root — this catches intermediate directory creation,
            // deletion, and atomic directory arrival.
            let root_candidates: Vec<PathBuf> = {
                let mut candidates = Vec::new();
                if let Some(parent) = normalized.parent()
                    && self.watch_roots.contains_key(parent)
                {
                    candidates.push(parent.to_path_buf());
                }
                if self.watch_roots.contains_key(&normalized) {
                    candidates.push(normalized.clone());
                }
                candidates
            };

            for root in root_candidates {
                // If the root itself was removed/renamed, invalidate the
                // backend watch by removing the root from the registry.
                // The targets are saved and reconciled afterward so they
                // migrate to a new root or get a fresh watch reinstalled.
                if is_remove && root == normalized {
                    let saved_targets: Vec<PathBuf> = self
                        .watch_roots
                        .get(&root)
                        .map(|s| s.targets.iter().cloned().collect())
                        .unwrap_or_default();
                    self.watch_roots.remove(&root);
                    for target in saved_targets {
                        self.reconcile_target(&target);
                    }
                } else {
                    self.reconcile_root(&root);
                }
            }
        }
    }

    fn handle_deadline(&mut self) {
        self.global_deadline = None;

        if self.pending_snapshots.is_empty() {
            return;
        }

        let mut all_ids: HashSet<String> = HashSet::new();
        for snapshot in self.pending_snapshots.values() {
            all_ids.extend(snapshot.iter().cloned());
        }
        self.pending_snapshots.clear();

        if !all_ids.is_empty() {
            let mut ids: Vec<String> = all_ids.into_iter().collect();
            ids.sort();
            let _ = self
                .event_tx
                .send(ProviderWatchEvent::ProvidersChanged(ids));
        }
    }
}
// ---------------------------------------------------------------------------
// Internal: spawn actor with a given backend (testable)
// ---------------------------------------------------------------------------

fn handle_command(actor: &mut ProviderWatchActor, cmd: ProviderWatchCommand) {
    match cmd {
        ProviderWatchCommand::ClearProvider { provider_id, ack } => {
            actor.handle_clear_provider(&provider_id);
            let _ = ack.send(());
        }
        ProviderWatchCommand::ReplaceProvider {
            provider_id,
            files,
            ack,
        } => {
            actor.handle_replace_provider(&provider_id, &files);
            let _ = ack.send(());
        }
    }
}

pub(crate) fn spawn_actor(
    backend: Box<dyn WatchBackend>,
    mut fs_event_rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
    mut cmd_rx: mpsc::UnboundedReceiver<ProviderWatchCommand>,
    event_tx: mpsc::UnboundedSender<ProviderWatchEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut actor = ProviderWatchActor {
            provider_files: HashMap::new(),
            targets: HashMap::new(),
            watch_roots: HashMap::new(),
            global_deadline: None,
            pending_snapshots: HashMap::new(),
            backend,
            event_tx,
        };

        loop {
            // Commands are prioritised over filesystem events and deadlines.
            if let Some(deadline) = actor.global_deadline {
                tokio::select! {
                    biased;
                    cmd = cmd_rx.recv() => {
                        let Some(cmd) = cmd else { break };
                        handle_command(&mut actor, cmd);
                    }
                    maybe_event = fs_event_rx.recv() => {
                        let Some(event) = maybe_event else { break };
                        actor.handle_fs_event(event);
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        actor.handle_deadline();
                    }
                }
            } else {
                tokio::select! {
                    biased;
                    cmd = cmd_rx.recv() => {
                        let Some(cmd) = cmd else { break };
                        handle_command(&mut actor, cmd);
                    }
                    maybe_event = fs_event_rx.recv() => {
                        let Some(event) = maybe_event else { break };
                        actor.handle_fs_event(event);
                    }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Public API: spawn the provider watch actor
// ---------------------------------------------------------------------------

/// Spawn the dynamic provider file-monitor actor.
///
/// Returns a command sender, an event receiver, and a join handle.
///
/// # Contract
///
/// - File paths should be supplied as absolute paths.  Relative paths are
///   normalized against the current working directory as a safety measure.
/// - Direct parents are watched non-recursively. Missing parents use a
///   non-recursive frontier at the nearest existing non-root ancestor and
///   migrate inward as directories appear.
/// - Watch failures for a specific file skip that file without crashing the
///   actor.
/// - Multiple files sharing the same parent share one OS watch.
/// - Commands are prioritised over filesystem events and debounce deadlines.
/// - **Acknowledgement semantics**: the `ack` sender on each command is
///   notified once the command has been fully processed by the actor's state
///   machine.  It does **not** guarantee that every individual filesystem
///   watch operation succeeded; per-file watch failures are logged and the
///   file is silently skipped.
pub fn spawn_provider_watch_actor() -> Result<(
    mpsc::UnboundedSender<ProviderWatchCommand>,
    mpsc::UnboundedReceiver<ProviderWatchEvent>,
    tokio::task::JoinHandle<()>,
)> {
    let (fs_event_tx, fs_event_rx) = mpsc::unbounded_channel::<notify::Result<Event>>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let watcher = RecommendedWatcher::new(
        move |event_result| {
            let _ = fs_event_tx.send(event_result);
        },
        Config::default(),
    )
    .context("failed to initialize filesystem watcher backend")?;

    let backend = NotifyBackend(watcher);
    let handle = spawn_actor(Box::new(backend), fs_event_rx, cmd_rx, event_tx);

    Ok((cmd_tx, event_rx, handle))
}
/// Shared async loop that drives the restart-only file-monitor state machine.
///
/// Reads events from `event_rx`, classifies them against `registrations`,
/// maintains a single debounce deadline for Restart, and emits the resolved
/// action on `action_tx`.  Exits when the event channel is closed.
async fn run_file_monitor_loop(
    registrations: Vec<WatchRegistration>,
    mut event_rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
    action_tx: mpsc::UnboundedSender<WatchAction>,
) {
    let mut restart_deadline: Option<tokio::time::Instant> = None;

    loop {
        if let Some(deadline) = restart_deadline {
            tokio::select! {
                maybe_event = event_rx.recv() => {
                    let Some(event_result) = maybe_event else {
                        log::warn!("filesystem watcher event channel closed unexpectedly");
                        break;
                    };

                    handle_event_general(
                        event_result,
                        &registrations,
                        &mut restart_deadline,
                    );
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let now = tokio::time::Instant::now();

                    if let Some(r) = restart_deadline
                        && r <= now
                    {
                        log::info!(
                            "no new filesystem changes for {}s; triggering daemon restart",
                            RESTART_DEBOUNCE_DELAY.as_secs(),
                        );
                        let _ = action_tx.send(WatchAction::Restart);
                        restart_deadline = None;
                    }
                }
            }
        } else {
            let Some(event_result) = event_rx.recv().await else {
                log::warn!("filesystem watcher event channel closed unexpectedly");
                break;
            };

            handle_event_general(event_result, &registrations, &mut restart_deadline);
        }
    }
}

/// A file dependency with its observed existence at subscription time.
#[derive(Debug, Clone)]
pub struct FileSubscription {
    pub path: PathBuf,
    /// Whether the file existed (was stat-able) at the moment subscribeFile
    /// was called during probe execution.
    pub existed_at_declaration: bool,
}

/// Result of attempting to start the provider watch actor.
///
/// On success all three fields are `Some`; on failure all are `None`.
/// This is the production conversion used by `run_daemon_mode` in main.rs.
pub struct ProviderMonitorParts {
    pub cmd_tx: Option<mpsc::UnboundedSender<ProviderWatchCommand>>,
    pub event_rx: Option<mpsc::UnboundedReceiver<ProviderWatchEvent>>,
    pub handle: Option<tokio::task::JoinHandle<()>>,
}

/// Result of a failed startup attempt — all components absent.
pub const MONITOR_FAILED: ProviderMonitorParts = ProviderMonitorParts {
    cmd_tx: None,
    event_rx: None,
    handle: None,
};

/// Attempt to start the provider watch actor and convert the result into
/// optional monitor components.
///
/// `factory` is called to create the actor.  In production this is
/// [`spawn_provider_watch_actor`]; tests inject a factory that returns
/// an error or a mock.  On success all three fields are `Some`; on failure
/// all are `None` (no dummy channels).
pub fn try_start_provider_monitor(
    factory: impl FnOnce() -> Result<(
        mpsc::UnboundedSender<ProviderWatchCommand>,
        mpsc::UnboundedReceiver<ProviderWatchEvent>,
        tokio::task::JoinHandle<()>,
    )>,
) -> ProviderMonitorParts {
    match factory() {
        Ok((tx, rx, handle)) => ProviderMonitorParts {
            cmd_tx: Some(tx),
            event_rx: Some(rx),
            handle: Some(handle),
        },
        Err(err) => {
            log::warn!(
                "provider file-monitor actor unavailable; continuing without dynamic provider dependency tracking: {}",
                err
            );
            ProviderMonitorParts {
                cmd_tx: None,
                event_rx: None,
                handle: None,
            }
        }
    }
}

/// Walk up from `path` to find the nearest existing ancestor directory,
/// excluding the filesystem root.  Returns `None` if only the root exists.
fn nearest_non_root_ancestor(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        if ancestor.is_dir() && ancestor.parent().is_some() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_interesting_event_kind(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Any | EventKind::Other => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        EventKind::Access(_) => false,
    }
}

fn format_event_details(event: &Event) -> String {
    format!(
        "kind={:?}, paths=[{}], attrs={:?}",
        event.kind,
        format_event_paths(&event.paths),
        event.attrs
    )
}

fn format_event_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| normalize_event_path(path).display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn add_file_watch(watch_roots: &mut Vec<WatchRoot>, file_path: &Path) -> Result<()> {
    let parent = file_path.parent().with_context(|| {
        format!(
            "cannot monitor file {} because it has no parent directory",
            file_path.display()
        )
    })?;

    if parent.exists() {
        add_watch_root(
            watch_roots,
            parent.to_path_buf(),
            RecursiveMode::NonRecursive,
        );
        return Ok(());
    }

    let fallback_root = nearest_existing_ancestor(parent).with_context(|| {
        format!(
            "cannot monitor file {} because no existing ancestor directory was found",
            file_path.display()
        )
    })?;

    log::debug!(
        "file parent directory {} does not exist yet; watching ancestor {} recursively",
        parent.display(),
        fallback_root.display()
    );

    add_watch_root(watch_roots, fallback_root, RecursiveMode::Recursive);
    Ok(())
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|candidate| candidate.exists())
        .map(Path::to_path_buf)
}

fn add_watch_root(watch_roots: &mut Vec<WatchRoot>, path: PathBuf, recursive: RecursiveMode) {
    if let Some(existing) = watch_roots.iter_mut().find(|root| root.path == path) {
        if existing.recursive == RecursiveMode::NonRecursive
            && recursive == RecursiveMode::Recursive
        {
            existing.recursive = RecursiveMode::Recursive;
        }
        return;
    }

    watch_roots.push(WatchRoot { path, recursive });
}

fn file_aliases(path: &Path) -> Vec<PathBuf> {
    let mut aliases = vec![path.to_path_buf()];
    if let Ok(canonical) = std::fs::canonicalize(path) {
        push_unique_path(&mut aliases, canonical);
    }
    aliases
}

fn path_matches_aliases(path: &Path, aliases: &[PathBuf]) -> bool {
    if aliases.iter().any(|candidate| candidate == path) {
        return true;
    }

    if let Ok(canonical) = std::fs::canonicalize(path)
        && aliases.iter().any(|candidate| candidate == &canonical)
    {
        return true;
    }

    false
}

fn normalize_existing_path(path: &Path, cwd: &Path) -> PathBuf {
    let absolute = normalize_absolute_path(path, cwd);
    std::fs::canonicalize(&absolute).unwrap_or(absolute)
}

fn normalize_absolute_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn normalize_event_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

fn push_unique_path(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !paths.iter().any(|existing| existing == &candidate) {
        paths.push(candidate);
    }
}

/// Normalize a file path for the actor: make absolute using lexical path
/// only (no canonicalization).  Subscription and event normalization are
/// symmetric — both use absolute lexical paths.
///
/// # Symlink semantics
///
/// The dynamic registry treats a subscribed symlink as an exact lexical path.
/// The actor watches the symlink's parent directory non-recursively, so only
/// changes to the symlink *entry itself* (create, remove, modify of the
/// directory entry) produce events.  Mutations of the symlink's *target*
/// (the file or directory the symlink points to) do **not** produce events
/// for the declared lexical path because the target lives outside the watched
/// parent.  This is intentional: the registry tracks the declared path, not
/// the resolved target.
fn normalize_file_path(path: &Path, cwd: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(cwd) = cwd {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

/// Test-only variant of `spawn_file_monitor` that accepts an injected event
/// channel instead of creating a real notify watcher.  The registrations are
/// used only for their classification rules (watch roots are ignored since
/// the caller injects events directly).  Delegates to the same shared
/// [`run_file_monitor_loop`] used by production [`spawn_file_monitor`].
#[cfg(test)]
pub(crate) fn spawn_file_monitor_with_events(
    registrations: Vec<WatchRegistration>,
    action_tx: mpsc::UnboundedSender<WatchAction>,
    event_rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_file_monitor_loop(registrations, event_rx, action_tx))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, MetadataKind, ModifyKind};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn event_with_path(kind: EventKind, path: &Path) -> Event {
        Event::new(kind).add_path(path.to_path_buf())
    }

    // =====================================================================
    // Restart-registration tests (preserved)
    // =====================================================================

    #[test]
    fn build_restart_registration_tracks_plugins_overrides_config_and_binary() {
        let temp = tempdir().expect("temp dir");
        let plugins_dir = temp.path().join("plugins");
        let overrides_dir = temp.path().join("plugin-overrides");
        let config_dir = temp.path().join("config");
        let bin_dir = temp.path().join("bin");

        std::fs::create_dir_all(&plugins_dir).expect("create plugins dir");
        std::fs::create_dir_all(&overrides_dir).expect("create overrides dir");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::create_dir_all(&bin_dir).expect("create bin dir");

        let config_file = config_dir.join("config.yaml");
        let binary_file = bin_dir.join("openusage-cli");
        std::fs::write(&config_file, "host: 127.0.0.1\n").expect("write config");
        std::fs::write(&binary_file, "binary").expect("write binary");

        let reg = build_restart_registration(RestartWatchInputs {
            plugins_dir: plugins_dir.clone(),
            plugin_overrides_dir: Some(overrides_dir.clone()),
            config_file: Some(config_file.clone()),
            binary_file: binary_file.clone(),
        })
        .expect("build restart registration");

        assert_eq!(reg.action, WatchAction::Restart);
        assert_eq!(reg.debounce, RESTART_DEBOUNCE_DELAY);
        assert!(reg.dir_roots.contains(&plugins_dir));
        assert!(reg.dir_roots.contains(&overrides_dir));
        assert!(reg.file_aliases.contains(&config_file));
        assert!(reg.file_aliases.contains(&binary_file));
    }

    #[test]
    fn build_restart_registration_fails_without_plugins_dir() {
        let temp = tempdir().expect("temp dir");
        let result = build_restart_registration(RestartWatchInputs {
            plugins_dir: temp.path().join("nonexistent-plugins"),
            plugin_overrides_dir: None,
            config_file: None,
            binary_file: temp.path().join("binary"),
        });
        assert!(result.is_err());
    }

    // =====================================================================
    // Event classification tests (preserved)
    // =====================================================================

    #[test]
    fn classify_path_for_registration_matches_dir_roots() {
        let mut reg = WatchRegistration::new(WatchAction::Restart, RESTART_DEBOUNCE_DELAY, "test");
        reg.add_dir_root(PathBuf::from("/tmp/plugins"), RecursiveMode::Recursive);

        let desc = classify_path_for_registration(Path::new("/tmp/plugins/codex/plugin.js"), &reg);
        assert!(desc.is_some(), "should match dir root");
        assert!(desc.unwrap().contains("test"));
    }

    #[test]
    fn classify_path_for_registration_matches_file_aliases() {
        let mut reg = WatchRegistration::new(WatchAction::Restart, RESTART_DEBOUNCE_DELAY, "test");
        reg.file_aliases.push(PathBuf::from("/tmp/config.yaml"));

        let desc = classify_path_for_registration(Path::new("/tmp/config.yaml"), &reg);
        assert!(desc.is_some(), "should match file alias");
    }

    #[test]
    fn classify_path_for_registration_returns_none_for_unwatched_path() {
        let reg = WatchRegistration::new(WatchAction::Restart, RESTART_DEBOUNCE_DELAY, "test");
        let desc = classify_path_for_registration(Path::new("/tmp/unrelated/file.txt"), &reg);
        assert!(desc.is_none());
    }

    // =====================================================================
    // Event kind filtering tests (preserved)
    // =====================================================================

    #[test]
    fn handle_event_general_ignores_access_events() {
        let reg = WatchRegistration::new(WatchAction::Restart, RESTART_DEBOUNCE_DELAY, "test");
        let mut restart_deadline = None;

        let event = event_with_path(
            EventKind::Access(AccessKind::Any),
            Path::new("/tmp/plugins/codex/plugin.js"),
        );

        handle_event_general(Ok(event), &[reg], &mut restart_deadline);

        assert!(restart_deadline.is_none());
    }

    #[test]
    fn handle_event_general_ignores_metadata_only_changes() {
        let reg = WatchRegistration::new(WatchAction::Restart, RESTART_DEBOUNCE_DELAY, "test");
        let mut restart_deadline = None;

        let event = event_with_path(
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            Path::new("/tmp/plugins/codex/plugin.js"),
        );

        handle_event_general(Ok(event), &[reg], &mut restart_deadline);

        assert!(restart_deadline.is_none());
    }

    // =====================================================================
    // Debounce / coalescing tests (preserved)
    // =====================================================================

    #[test]
    fn restart_path_changes_remain_restart_only() {
        let mut reg = WatchRegistration::new(WatchAction::Restart, RESTART_DEBOUNCE_DELAY, "test");
        reg.add_dir_root(PathBuf::from("/tmp/plugins"), RecursiveMode::Recursive);

        let mut restart_deadline = None;

        let event = event_with_path(
            EventKind::Create(CreateKind::Any),
            Path::new("/tmp/plugins/codex/plugin.js"),
        );

        handle_event_general(Ok(event), &[reg], &mut restart_deadline);

        assert!(restart_deadline.is_some(), "restart deadline should be set");
    }

    // =====================================================================
    // add_watch_root tests (preserved)
    // =====================================================================

    #[test]
    fn add_watch_root_upgrades_non_recursive_to_recursive() {
        let mut roots = vec![WatchRoot {
            path: PathBuf::from("/tmp/config"),
            recursive: RecursiveMode::NonRecursive,
        }];

        add_watch_root(
            &mut roots,
            PathBuf::from("/tmp/config"),
            RecursiveMode::Recursive,
        );

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].recursive, RecursiveMode::Recursive);
    }

    // =====================================================================
    // build_restart_registration watch-plan semantics (preserved)
    // =====================================================================

    #[test]
    fn build_restart_registration_preserves_watch_plan_semantics() {
        let temp = tempdir().expect("temp dir");
        let plugins_dir = temp.path().join("plugins");
        let overrides_dir = temp.path().join("plugin-overrides");
        let config_dir = temp.path().join("config");
        let bin_dir = temp.path().join("bin");

        std::fs::create_dir_all(&plugins_dir).expect("create plugins dir");
        std::fs::create_dir_all(&overrides_dir).expect("create overrides dir");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::create_dir_all(&bin_dir).expect("create bin dir");

        let config_file = config_dir.join("config.yaml");
        let binary_file = bin_dir.join("openusage-cli");
        std::fs::write(&config_file, "host: 127.0.0.1\n").expect("write config");
        std::fs::write(&binary_file, "binary").expect("write binary");

        let reg = build_restart_registration(RestartWatchInputs {
            plugins_dir: plugins_dir.clone(),
            plugin_overrides_dir: Some(overrides_dir.clone()),
            config_file: Some(config_file.clone()),
            binary_file: binary_file.clone(),
        })
        .expect("build restart registration");

        // Verify dir_roots contain expected paths
        assert!(reg.dir_roots.contains(&plugins_dir));
        assert!(reg.dir_roots.contains(&overrides_dir));

        // Verify file_aliases contain expected files
        assert!(reg.file_aliases.contains(&config_file));
        assert!(reg.file_aliases.contains(&binary_file));

        // Verify watch_roots contain parent directories for files
        let config_parent = config_file.parent().expect("config parent");
        let binary_parent = binary_file.parent().expect("binary parent");
        let has_config_parent = reg
            .watch_roots
            .iter()
            .any(|wr| wr.path == config_parent && wr.recursive == RecursiveMode::NonRecursive);
        let has_binary_parent = reg
            .watch_roots
            .iter()
            .any(|wr| wr.path == binary_parent && wr.recursive == RecursiveMode::NonRecursive);
        assert!(
            has_config_parent,
            "config parent should be watched non-recursively"
        );
        assert!(
            has_binary_parent,
            "binary parent should be watched non-recursively"
        );
    }

    // =====================================================================
    // Provider watch actor tests
    // =====================================================================

    #[allow(clippy::type_complexity)]
    /// Helper: create a fake backend and spawn the actor, returning channels
    /// and the fake backend for inspection.
    fn spawn_test_actor() -> (
        mpsc::UnboundedSender<ProviderWatchCommand>,
        mpsc::UnboundedReceiver<ProviderWatchEvent>,
        mpsc::UnboundedSender<notify::Result<Event>>,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<FakeBackend>>,
    ) {
        let (fs_event_tx, fs_event_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let backend = std::sync::Arc::new(std::sync::Mutex::new(FakeBackend::new()));
        let backend_clone = Arc::clone(&backend);

        // Wrap in a backend that delegates to the fake.
        struct SharedFakeBackend(std::sync::Arc<std::sync::Mutex<FakeBackend>>);
        impl WatchBackend for SharedFakeBackend {
            fn watch(&mut self, path: &Path, recursive: RecursiveMode) -> notify::Result<()> {
                self.0.lock().unwrap().watch(path, recursive)
            }
            fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
                self.0.lock().unwrap().unwatch(path)
            }
        }

        let handle = spawn_actor(
            Box::new(SharedFakeBackend(backend_clone)),
            fs_event_rx,
            cmd_rx,
            event_tx,
        );

        (cmd_tx, event_rx, fs_event_tx, handle, backend)
    }

    #[tokio::test]
    async fn shared_subscription_installed_once() {
        let (cmd_tx, _event_rx, _fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("auth.json");
        let file_b = dir.path().join("config.json");
        std::fs::write(&file_a, "a").expect("write");
        std::fs::write(&file_b, "b").expect("write");

        // Both files share the same parent.
        let (ack_tx, ack_rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![
                    FileSubscription {
                        path: file_a.clone(),
                        existed_at_declaration: true,
                    },
                    FileSubscription {
                        path: file_b.clone(),
                        existed_at_declaration: true,
                    },
                ],
                ack: ack_tx,
            })
            .expect("send");
        ack_rx.await.expect("ack");

        let backend = backend.lock().unwrap();
        // Only one watch for the shared parent.
        assert_eq!(backend.watched.len(), 1);
        assert!(backend.watched.contains_key(dir.path()));

        handle.abort();
    }

    #[tokio::test]
    async fn repeated_registration_adds_more_files() {
        let (cmd_tx, _event_rx, _fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        let file_b = dir.path().join("b.json");
        std::fs::write(&file_a, "a").expect("write");
        std::fs::write(&file_b, "b").expect("write");

        // Register first file.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Register second file (replaces).
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![
                    FileSubscription {
                        path: file_a.clone(),
                        existed_at_declaration: true,
                    },
                    FileSubscription {
                        path: file_b.clone(),
                        existed_at_declaration: true,
                    },
                ],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        let backend = backend.lock().unwrap();
        // One parent, one watch.
        assert_eq!(backend.watched.len(), 1);

        handle.abort();
    }

    #[tokio::test]
    async fn multiple_files_share_parent() {
        let (cmd_tx, _event_rx, _fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        let file_b = dir.path().join("b.json");
        std::fs::write(&file_a, "a").expect("write");
        std::fs::write(&file_b, "b").expect("write");

        // Two providers, different files, same parent.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p2".into(),
                files: vec![FileSubscription {
                    path: file_b.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        let backend = backend.lock().unwrap();
        assert_eq!(backend.watched.len(), 1);

        handle.abort();
    }

    #[tokio::test]
    async fn clear_one_provider_keeps_shared_watch() {
        let (cmd_tx, _event_rx, _fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        let file_b = dir.path().join("b.json");
        std::fs::write(&file_a, "a").expect("write");
        std::fs::write(&file_b, "b").expect("write");

        // Two providers, same file.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p2".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Clear one provider.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ClearProvider {
                provider_id: "p1".into(),
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Watch should still exist (p2 still needs it).
        let backend = backend.lock().unwrap();
        assert_eq!(backend.watched.len(), 1);

        handle.abort();
    }

    #[tokio::test]
    async fn clear_last_provider_removes_watch() {
        let (cmd_tx, _event_rx, _fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        std::fs::write(&file_a, "a").expect("write");

        // One provider, one file.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Clear the provider.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ClearProvider {
                provider_id: "p1".into(),
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Watch should be removed.
        let backend = backend.lock().unwrap();
        assert!(backend.watched.is_empty());

        handle.abort();
    }

    #[tokio::test]
    async fn replacement_atomically_replaces_state() {
        let (cmd_tx, _event_rx, _fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        let file_b = dir.path().join("b.json");
        std::fs::write(&file_a, "a").expect("write");
        std::fs::write(&file_b, "b").expect("write");

        // Set initial files.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Replace with different file.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_b.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Only file_b's watch should remain.
        let backend = backend.lock().unwrap();
        assert_eq!(backend.watched.len(), 1);
        assert!(backend.watched.contains_key(dir.path()));

        handle.abort();
    }

    #[tokio::test]
    async fn empty_replacement_clears_all() {
        let (cmd_tx, _event_rx, _fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        std::fs::write(&file_a, "a").expect("write");

        // Set initial file.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Replace with empty (no dependencies).
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        let backend = backend.lock().unwrap();
        assert!(backend.watched.is_empty());

        handle.abort();
    }

    #[tokio::test]
    async fn missing_parent_directory_establishes_frontier_watch() {
        // When the parent directory does not exist, the actor now establishes
        // a frontier watch on the nearest existing ancestor instead of skipping.
        let (cmd_tx, _event_rx, _fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("nonexistent/opencode/auth.json");
        // Do NOT create the parent.

        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a,
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // A frontier watch should have been installed on the nearest existing
        // ancestor (the temp dir), not skipped.
        let backend = backend.lock().unwrap();
        assert!(
            !backend.watched.is_empty(),
            "expected frontier watch on nearest existing ancestor"
        );
        assert!(
            backend.watched.contains_key(dir.path()),
            "expected frontier watch on temp dir, got: {:?}",
            backend.watched.keys().collect::<Vec<_>>()
        );

        handle.abort();
    }

    #[tokio::test]
    async fn watch_failure_skips_file() {
        let (cmd_tx, _event_rx, _fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        std::fs::write(&file_a, "a").expect("write");

        // Make the backend fail the next watch.
        {
            let mut b = backend.lock().unwrap();
            b.fail_next_watch = Some(notify::Error::new(notify::ErrorKind::Generic(
                "injected failure".to_string(),
            )));
        }

        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a,
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // No watch should have been installed.
        let backend = backend.lock().unwrap();
        assert!(backend.watched.is_empty());

        handle.abort();
    }

    #[tokio::test]
    async fn debounce_fan_out_unions_providers() {
        let (cmd_tx, mut event_rx, fs_tx, handle, _backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        let file_b = dir.path().join("b.json");
        std::fs::write(&file_a, "a").expect("write");
        std::fs::write(&file_b, "b").expect("write");

        // p1 watches file_a, p2 watches file_b.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p2".into(),
                files: vec![FileSubscription {
                    path: file_b.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Send modify events for both files.
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Modify(ModifyKind::Any),
                &file_a,
            )))
            .expect("send");
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Modify(ModifyKind::Any),
                &file_b,
            )))
            .expect("send");

        // Wait for debounce (3s) + margin.
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout")
            .expect("event");

        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids.len(), 2);
                assert_eq!(ids, vec!["p1".to_string(), "p2".to_string()]);
            }
        }

        handle.abort();
    }

    #[tokio::test]
    async fn clear_provider_removes_pending_event() {
        let (cmd_tx, mut event_rx, fs_tx, handle, _backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        std::fs::write(&file_a, "a").expect("write");

        // Register provider.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Send a modify event.
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Modify(ModifyKind::Any),
                &file_a,
            )))
            .expect("send");

        // Clear the provider before the debounce fires.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ClearProvider {
                provider_id: "p1".into(),
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // No event should be emitted (provider was cleared).
        let result = tokio::time::timeout(Duration::from_secs(4), event_rx.recv()).await;
        assert!(result.is_err(), "no event should be emitted after clearing");

        handle.abort();
    }

    #[tokio::test]
    async fn ignored_event_kinds_do_not_trigger_debounce() {
        let (cmd_tx, mut event_rx, fs_tx, handle, _backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        std::fs::write(&file_a, "a").expect("write");

        // Register provider.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Send an access event (should be ignored).
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Access(AccessKind::Any),
                &file_a,
            )))
            .expect("send");

        // Send a metadata-only modify event (should be ignored).
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
                &file_a,
            )))
            .expect("send");

        // No event should be emitted.
        let result = tokio::time::timeout(Duration::from_secs(4), event_rx.recv()).await;
        assert!(
            result.is_err(),
            "no event should be emitted for ignored kinds"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn clear_and_replace_does_not_resurrect_pending_provider() {
        // p1 and p2 share file_a.  Event snapshots both.  Clear p1, then
        // ReplaceProvider p1 with same file.  The pending snapshot from the
        // earlier event must not be resurrected — expiry emits only p2.
        let (cmd_tx, mut event_rx, fs_tx, handle, _backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        std::fs::write(&file_a, "a").expect("write");

        // Both providers watch the same file.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p2".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Send a modify event — snapshots both p1 and p2.
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Modify(ModifyKind::Any),
                &file_a,
            )))
            .expect("send");

        // Clear p1 — removes p1 from the pending snapshot.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ClearProvider {
                provider_id: "p1".into(),
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Replace p1 with the same file — must not resurrect the snapshot.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Wait for debounce expiry.
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout")
            .expect("event");

        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["p2".to_string()], "only p2 should be emitted");
            }
        }

        handle.abort();
    }

    #[tokio::test]
    async fn unwatch_failure_retains_installed_root_for_resubscribe() {
        let (cmd_tx, _event_rx, _fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        std::fs::write(&file_a, "a").expect("write");

        // Register provider.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Make the backend fail the next unwatch.
        {
            let mut b = backend.lock().unwrap();
            b.fail_next_unwatch = Some(notify::Error::new(notify::ErrorKind::Generic(
                "injected unwatch failure".to_string(),
            )));
        }

        // Clear the provider — unwatch fails, but installed root is retained.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ClearProvider {
                provider_id: "p1".into(),
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // The watch should still be in the backend (unwatch failed).
        {
            let b = backend.lock().unwrap();
            assert!(
                b.watched.contains_key(dir.path()),
                "watch root should be retained after failed unwatch"
            );
        }

        // Resubscribe with the same file — should NOT call watch again.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: file_a.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // The watch count should still be 1 (no duplicate watch).
        {
            let b = backend.lock().unwrap();
            assert_eq!(
                b.watched.len(),
                1,
                "should reuse existing watch root, not add a duplicate"
            );
        }

        handle.abort();
    }

    #[tokio::test]
    async fn normalization_uses_lexical_absolute_paths() {
        // Verify that normalize_file_path does NOT canonicalize.
        let dir = tempdir().expect("temp dir");
        let file_a = dir.path().join("a.json");
        std::fs::write(&file_a, "a").expect("write");

        // Create a symlink to the file (if supported).
        let link = dir.path().join("link.json");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&file_a, &link).expect("symlink");
        #[cfg(not(unix))]
        std::fs::hard_link(&file_a, &link).expect("hard link");

        // normalize_file_path should return the lexical path, not the
        // canonical (resolved) one.
        let normalized = normalize_file_path(&link, None);
        assert_eq!(
            normalized, link,
            "must preserve lexical path, not canonicalize"
        );

        // The event path normalization is also lexical.
        let event_path = normalize_event_path(&link);
        assert_eq!(event_path, link, "event normalization must also be lexical");
    }

    // =====================================================================
    // Real notify backend: atomic rename
    // =====================================================================

    #[tokio::test]
    async fn real_notify_atomic_rename_produces_providers_changed() {
        // Use the real notify backend to prove that atomically renaming a
        // temp file over a registered path triggers a debounced event.
        let dir = tempdir().expect("temp dir");
        let auth_file = dir.path().join("auth.json");
        std::fs::write(&auth_file, "initial").expect("write initial");

        let (cmd_tx, mut event_rx, handle) =
            spawn_provider_watch_actor().expect("spawn real actor");

        // Register the file.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "test-provider".into(),
                files: vec![FileSubscription {
                    path: auth_file.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send replace");
        rx.await.expect("ack");

        // Atomically replace the file via rename.
        let tmp = dir.path().join("auth.json.tmp");
        std::fs::write(&tmp, "updated").expect("write tmp");
        std::fs::rename(&tmp, &auth_file).expect("atomic rename");

        // Wait for the debounced event (one-second debounce + margin).
        let event = tokio::time::timeout(Duration::from_secs(6), event_rx.recv())
            .await
            .expect("timeout waiting for ProvidersChanged after atomic rename")
            .expect("channel closed");

        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["test-provider".to_string()]);
            }
        }

        handle.abort();
        handle.abort();
    }

    // =====================================================================
    // Real notify: spawn_file_monitor watcher lifetime regression
    // =====================================================================

    #[tokio::test]
    async fn real_notify_spawn_file_monitor_watcher_lifetime() {
        // Prove that the RecommendedWatcher inside spawn_file_monitor stays
        // alive for the lifetime of the spawned task.  If the watcher drops
        // early (the regression), no action will arrive.
        let dir = tempdir().expect("temp dir");
        let watched_file = dir.path().join("watch-me.json");
        std::fs::write(&watched_file, "initial").expect("write initial");

        let mut reg = WatchRegistration::new(
            WatchAction::Restart,
            Duration::from_millis(100),
            "lifetime-test",
        );
        reg.add_file(&watched_file).expect("add file");

        let (action_tx, mut action_rx) = mpsc::unbounded_channel();
        let handle = spawn_file_monitor(vec![reg], action_tx).expect("spawn_file_monitor");

        // Atomically replace the watched file.
        let tmp = dir.path().join("watch-me.json.tmp");
        std::fs::write(&tmp, "updated").expect("write tmp");
        std::fs::rename(&tmp, &watched_file).expect("atomic rename");

        // Wait for the debounced action (100ms debounce + margin).
        let action = tokio::time::timeout(Duration::from_secs(4), action_rx.recv())
            .await
            .expect("timeout waiting for WatchAction — watcher likely dropped early")
            .expect("channel closed");

        assert_eq!(action, WatchAction::Restart);

        handle.abort();
        let _ = handle.await;
    }

    // =====================================================================
    // Real Unix symlink: register lexical path, atomically replace symlink

    // =====================================================================
    // Real Unix symlink: register lexical path, atomically replace symlink
    // =====================================================================

    #[tokio::test]
    #[cfg(unix)]
    async fn real_symlink_atomic_replace_produces_providers_changed() {
        use std::os::unix::fs as unix_fs;

        let dir = tempdir().expect("temp dir");
        let target_dir = dir.path().join("targets");
        std::fs::create_dir_all(&target_dir).expect("create targets dir");

        let target_a = target_dir.join("config-v1.json");
        let target_b = target_dir.join("config-v2.json");
        std::fs::write(&target_a, "v1").expect("write v1");
        std::fs::write(&target_b, "v2").expect("write v2");

        // Create a symlink pointing to v1.
        let link = dir.path().join("current-config.json");
        unix_fs::symlink(&target_a, &link).expect("create symlink");

        let (cmd_tx, mut event_rx, handle) =
            spawn_provider_watch_actor().expect("spawn real actor");

        // Register the lexical symlink path (not the canonical target).
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "symlink-provider".into(),
                files: vec![FileSubscription {
                    path: link.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send replace");
        rx.await.expect("ack");

        // Atomically replace the symlink in its parent directory.
        let new_link = dir.path().join("current-config.json.new");
        unix_fs::symlink(&target_b, &new_link).expect("create new symlink");
        std::fs::rename(&new_link, &link).expect("atomic rename symlink");

        // Wait for the debounced event.
        let event = tokio::time::timeout(Duration::from_secs(6), event_rx.recv())
            .await
            .expect("timeout waiting for ProvidersChanged after symlink replace")
            .expect("channel closed");

        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["symlink-provider".to_string()]);
            }
        }

        handle.abort();
    }

    // =====================================================================
    // Symlink target mutation regression test
    // =====================================================================

    #[tokio::test]
    #[cfg(unix)]
    async fn symlink_target_mutation_does_not_emit_refresh() {
        // The dynamic registry treats a subscribed symlink as an exact
        // lexical path.  Mutating the symlink's *target* (the file it
        // points to) must NOT produce an event for the declared lexical
        // path, because the target lives outside the watched parent.
        use std::os::unix::fs as unix_fs;

        let dir = tempdir().expect("temp dir");
        let target_dir = dir.path().join("targets");
        std::fs::create_dir_all(&target_dir).expect("create targets dir");

        let target = target_dir.join("real-data.json");
        std::fs::write(&target, "v1").expect("write target");

        // Create a symlink pointing to the target.
        let link = dir.path().join("current.json");
        unix_fs::symlink(&target, &link).expect("create symlink");

        let (cmd_tx, mut event_rx, handle) =
            spawn_provider_watch_actor().expect("spawn real actor");

        // Register the lexical symlink path.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "link-provider".into(),
                files: vec![FileSubscription {
                    path: link.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send replace");
        rx.await.expect("ack");

        // Mutate the symlink target (not the symlink entry itself).
        std::fs::write(&target, "v2").expect("write target v2");

        // No event should be emitted for the registered lexical path.
        let result = tokio::time::timeout(Duration::from_secs(4), event_rx.recv()).await;
        assert!(
            result.is_err(),
            "no event should be emitted for target mutation of a registered symlink"
        );

        handle.abort();
    }

    // =====================================================================
    // Static file-monitor scheduling tests (injected event channel)
    // =====================================================================

    #[tokio::test]
    async fn file_monitor_event_channel_close_does_not_hang() {
        let (action_tx, _action_rx) = mpsc::unbounded_channel::<WatchAction>();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let reg =
            WatchRegistration::new(WatchAction::Restart, Duration::from_secs(300), "hang-test");

        let handle = spawn_file_monitor_with_events(vec![reg], action_tx, event_rx);

        // Close the event input channel.
        drop(event_tx);

        // The monitor task should exit promptly (break on channel close).
        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("monitor task should exit within timeout after event channel close");

        result.expect("monitor task completed");
    }

    #[tokio::test]
    async fn file_monitor_action_channel_close_does_not_hang() {
        let (action_tx, action_rx) = mpsc::unbounded_channel::<WatchAction>();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let reg = WatchRegistration::new(
            WatchAction::Restart,
            Duration::from_millis(50),
            "action-hang-test",
        );

        let handle = spawn_file_monitor_with_events(vec![reg], action_tx, event_rx);

        // Drop the action receiver so sends fail silently.
        drop(action_rx);

        // Send an event to trigger a deadline.
        event_tx
            .send(Ok(event_with_path(
                EventKind::Modify(ModifyKind::Any),
                Path::new("/tmp/action-hang-test/file.js"),
            )))
            .expect("send event");

        // The monitor should not hang — it should process the event, try to
        // send on the closed action channel (which fails silently), and
        // continue.  Close the event channel to make it exit.
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(event_tx);

        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("monitor task should exit within timeout after action channel close");

        result.expect("monitor task completed");
    }

    // =====================================================================
    // Frontier: missing parent → empty intermediate dir → anchor migrates
    // =====================================================================

    #[tokio::test]
    async fn frontier_missing_parent_dir_created_then_file_created() {
        let (cmd_tx, mut event_rx, fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let intermediate = dir.path().join("a");
        let target = intermediate.join("b.json");

        // Register target when parent is missing.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: target.clone(),
                    existed_at_declaration: false,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Assert initial watch on the temp dir root.
        {
            let b = backend.lock().unwrap();
            assert!(
                b.watched.contains_key(dir.path()),
                "initial watch should be on temp dir root"
            );
            assert_eq!(b.watched.len(), 1, "only one watch initially");
        }

        // Create the intermediate directory (root event).
        std::fs::create_dir(&intermediate).expect("create intermediate");
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Create(notify::event::CreateKind::Any),
                &intermediate,
            )))
            .expect("send event");

        // Allow a brief moment for the actor to process the event.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Assert that the old root was released and the new direct parent
        // watch was installed BEFORE any target-file event.
        {
            let b = backend.lock().unwrap();
            assert!(
                !b.watched.contains_key(dir.path()),
                "old root should be released after intermediate dir creation"
            );
            assert!(
                b.watched.contains_key(&intermediate),
                "new watch should be on intermediate dir (direct parent)"
            );
            assert_eq!(b.watched.len(), 1, "only one watch should remain");
        }

        // No event should fire yet — target still doesn't exist.
        let premature = tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await;
        assert!(premature.is_err(), "no refresh before target file exists");

        // Now create the target file.
        std::fs::write(&target, "data").expect("write target");
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Create(notify::event::CreateKind::Any),
                &target,
            )))
            .expect("send event");

        // Should receive a refresh.
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout waiting for ProvidersChanged after file creation")
            .expect("channel closed");

        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["p1".to_string()]);
            }
        }

        handle.abort();
    }

    // =====================================================================
    // Frontier: populated directory atomically arrives with target
    // =====================================================================

    #[tokio::test]
    async fn frontier_populated_directory_atomic_arrival() {
        let (cmd_tx, mut event_rx, fs_tx, handle, _backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let target = dir.path().join("a/b/c/data.json");

        // Register target when nothing exists.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: target.clone(),
                    existed_at_declaration: false,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Atomically create the full directory tree with the target file.
        std::fs::create_dir_all(dir.path().join("a/b/c")).expect("create dirs");
        std::fs::write(&target, "data").expect("write target");

        // Send a create event on the first child of the watched root (dir).
        // The root is dir.path() — the nearest existing ancestor.
        let first_child = dir.path().join("a");
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Create(notify::event::CreateKind::Any),
                &first_child,
            )))
            .expect("send event");

        // Should receive a refresh because target now exists.
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout waiting for ProvidersChanged after atomic arrival")
            .expect("channel closed");

        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["p1".to_string()]);
            }
        }

        handle.abort();
    }

    // =====================================================================
    // Frontier: creation → deletion → recreation gives distinct refreshes
    // =====================================================================

    #[tokio::test]
    async fn frontier_create_delete_recreate_distinct_refreshes() {
        let (cmd_tx, mut event_rx, fs_tx, handle, _backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let target = dir.path().join("watch.json");

        // Register existing target.
        std::fs::write(&target, "v1").expect("write");
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: target.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Drain the declaration-to-install race event if any.
        tokio::time::timeout(Duration::from_millis(200), event_rx.recv())
            .await
            .ok();

        // Delete the target.
        std::fs::remove_file(&target).expect("remove");
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Remove(notify::event::RemoveKind::Any),
                &target,
            )))
            .expect("send event");

        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout waiting for deletion refresh")
            .expect("channel closed");
        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["p1".to_string()]);
            }
        }

        // Recreate the target.
        std::fs::write(&target, "v2").expect("write");
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Create(notify::event::CreateKind::Any),
                &target,
            )))
            .expect("send event");

        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout waiting for recreation refresh")
            .expect("channel closed");
        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["p1".to_string()]);
            }
        }

        // No third event.
        let third = tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await;
        assert!(third.is_err(), "no third refresh after deletion+recreation");

        handle.abort();
    }

    // =====================================================================
    // Frontier: direct parent deletion migrates outward; recreation inwards
    // =====================================================================

    #[tokio::test]
    async fn frontier_parent_deleted_then_recreated_with_file() {
        let (cmd_tx, mut event_rx, fs_tx, handle, _backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let parent = dir.path().join("sub");
        let target = parent.join("data.json");

        // Create parent + target.
        std::fs::create_dir(&parent).expect("create parent");
        std::fs::write(&target, "v1").expect("write target");

        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: target.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Drain race event.
        tokio::time::timeout(Duration::from_millis(200), event_rx.recv())
            .await
            .ok();

        // Delete the parent directory (and thus the target).
        std::fs::remove_dir_all(&parent).expect("remove parent");
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Remove(notify::event::RemoveKind::Any),
                &parent,
            )))
            .expect("send event");

        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout waiting for parent deletion refresh")
            .expect("channel closed");
        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["p1".to_string()]);
            }
        }

        // Recreate parent + target.
        std::fs::create_dir(&parent).expect("recreate parent");
        std::fs::write(&target, "v2").expect("rewrite target");
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Create(notify::event::CreateKind::Any),
                &parent,
            )))
            .expect("send event");

        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout waiting for parent recreation refresh")
            .expect("channel closed");
        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["p1".to_string()]);
            }
        }

        handle.abort();
    }

    // =====================================================================
    // Frontier: two providers share one initially missing target
    // =====================================================================

    #[tokio::test]
    async fn frontier_two_providers_share_missing_target() {
        let (cmd_tx, mut event_rx, fs_tx, handle, _backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let target = dir.path().join("shared.json");

        // Both providers register the same missing target.
        for pid in &["p1", "p2"] {
            let (ack, rx) = oneshot::channel();
            cmd_tx
                .send(ProviderWatchCommand::ReplaceProvider {
                    provider_id: (*pid).to_string(),
                    files: vec![FileSubscription {
                        path: target.clone(),
                        existed_at_declaration: false,
                    }],
                    ack,
                })
                .expect("send");
            rx.await.expect("ack");
        }

        // Create the target.
        std::fs::write(&target, "data").expect("write");
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Create(notify::event::CreateKind::Any),
                &target,
            )))
            .expect("send event");

        // Both providers should be fanned out.
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout waiting for shared target refresh")
            .expect("channel closed");
        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                let mut sorted = ids.clone();
                sorted.sort();
                assert_eq!(sorted, vec!["p1".to_string(), "p2".to_string()]);
            }
        }

        // Clear p1 — p2 should still be monitored.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ClearProvider {
                provider_id: "p1".into(),
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Delete the target — only p2 should fire.
        std::fs::remove_file(&target).expect("remove");
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Remove(notify::event::RemoveKind::Any),
                &target,
            )))
            .expect("send event");

        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout waiting after clear")
            .expect("channel closed");
        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["p2".to_string()]);
            }
        }

        handle.abort();
    }

    // =====================================================================
    // Frontier: direct-parent target and frontier target share same OS root
    // =====================================================================

    #[tokio::test]
    async fn frontier_direct_and_frontier_share_root() {
        let (cmd_tx, _event_rx, _fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");

        // Direct parent target.
        let file_a = dir.path().join("a.json");
        std::fs::write(&file_a, "a").expect("write");

        // Frontier target (parent missing).
        let file_b = dir.path().join("missing/b.json");

        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![
                    FileSubscription {
                        path: file_a,
                        existed_at_declaration: true,
                    },
                    FileSubscription {
                        path: file_b,
                        existed_at_declaration: false,
                    },
                ],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Both should share the same root (dir.path()) — only one OS watch installed.
        let backend = backend.lock().unwrap();
        assert_eq!(
            backend.watched.len(),
            1,
            "expected exactly one watch root for both targets"
        );
        assert!(
            backend.watched.contains_key(dir.path()),
            "expected watch on dir root"
        );

        handle.abort();
    }

    // =====================================================================
    // Frontier: deeper-root watch failure retains existing frontier
    // =====================================================================

    #[tokio::test]
    async fn frontier_deeper_watch_failure_retains_old_root() {
        let (cmd_tx, mut event_rx, fs_tx, handle, backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let target = dir.path().join("a/b/data.json");

        // Register with missing parent.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: target.clone(),
                    existed_at_declaration: false,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Verify watch on dir.
        {
            let b = backend.lock().unwrap();
            assert!(b.watched.contains_key(dir.path()), "initial watch on dir");
        }

        // Create "a" directory.
        let a_dir = dir.path().join("a");
        std::fs::create_dir(&a_dir).expect("create a");

        // Make the backend fail the next watch.
        {
            let mut b = backend.lock().unwrap();
            b.fail_next_watch = Some(notify::Error::new(notify::ErrorKind::Generic(
                "injected failure".to_string(),
            )));
        }

        // Send event for "a" — should try to migrate inward but fail.
        fs_tx
            .send(Ok(event_with_path(
                EventKind::Create(notify::event::CreateKind::Any),
                &a_dir,
            )))
            .expect("send event");

        // No refresh should fire because migration failed and target still absent.
        let premature = tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await;
        assert!(premature.is_err(), "no refresh after failed migration");

        // Old watch on dir should still be present.
        {
            let b = backend.lock().unwrap();
            assert!(
                b.watched.contains_key(dir.path()),
                "old root retained after failed migration"
            );
        }

        // Now create b/ and data.json, then send event on "a" again.
        let b_dir = a_dir.join("b");
        std::fs::create_dir(&b_dir).expect("create b");
        std::fs::write(&target, "data").expect("write target");

        fs_tx
            .send(Ok(event_with_path(
                EventKind::Modify(notify::event::ModifyKind::Any),
                &a_dir,
            )))
            .expect("send event");

        // Should now reconcile and fire refresh.
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout waiting for refresh after eventual success")
            .expect("channel closed");
        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["p1".to_string()]);
            }
        }

        handle.abort();
    }

    // =====================================================================
    // Declaration-to-install race: declared missing but present at install
    // =====================================================================

    #[tokio::test]
    async fn race_declared_missing_present_at_install() {
        let (cmd_tx, mut event_rx, _fs_tx, handle, _backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let target = dir.path().join("race.json");

        // File exists BEFORE ReplaceProvider.
        std::fs::write(&target, "data").expect("write");

        // Declare it as missing.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: target.clone(),
                    existed_at_declaration: false, // declared missing
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Should get exactly one refresh for the race.
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout waiting for race refresh (declared missing)")
            .expect("channel closed");
        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["p1".to_string()]);
            }
        }

        // No second event.
        let second = tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await;
        assert!(second.is_err(), "no duplicate race refresh");

        handle.abort();
    }

    // =====================================================================
    // Declaration-to-install race: declared present but missing at install
    // =====================================================================

    #[tokio::test]
    async fn race_declared_present_missing_at_install() {
        let (cmd_tx, mut event_rx, _fs_tx, handle, _backend) = spawn_test_actor();
        let dir = tempdir().expect("temp dir");
        let target = dir.path().join("race2.json");

        // File does NOT exist at ReplaceProvider time.
        // Declare it as present.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "p1".into(),
                files: vec![FileSubscription {
                    path: target.clone(),
                    existed_at_declaration: true, // declared present, but actually missing
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Should get exactly one refresh for the race.
        let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timeout waiting for race refresh (declared present)")
            .expect("channel closed");
        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["p1".to_string()]);
            }
        }

        // No second event.
        let second = tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await;
        assert!(second.is_err(), "no duplicate race refresh");

        handle.abort();
    }

    // =====================================================================
    // Real notify: missing-file frontier lifecycle
    // =====================================================================

    #[tokio::test]
    async fn real_notify_frontier_missing_file_lifecycle() {
        let dir = tempdir().expect("temp dir");
        let target = dir.path().join("a/b/data.json");

        let (cmd_tx, mut event_rx, handle) =
            spawn_provider_watch_actor().expect("spawn real actor");

        // Register target whose parent does not exist.
        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "frontier-p1".into(),
                files: vec![FileSubscription {
                    path: target.clone(),
                    existed_at_declaration: false,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Create directories and file.
        std::fs::create_dir_all(target.parent().unwrap()).expect("create dirs");
        std::fs::write(&target, "data").expect("write target");

        // Wait for the debounced event (one-second debounce + margin).
        let event = tokio::time::timeout(Duration::from_secs(6), event_rx.recv())
            .await
            .expect("timeout waiting for ProvidersChanged after frontier lifecycle")
            .expect("channel closed");

        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["frontier-p1".to_string()]);
            }
        }

        handle.abort();
    }

    // =====================================================================
    // Real notify: delete parent/file, recreate, observe second refresh
    // =====================================================================

    #[tokio::test]
    async fn real_notify_parent_delete_recreate_lifecycle() {
        // Register a target under an existing direct parent.  Delete the
        // parent (and thus the target), observe migration outward and a
        // refresh.  Recreate parent/file, observe a second refresh.
        // This must fail if a stale root is treated as actively watched.
        let dir = tempdir().expect("temp dir");
        let parent = dir.path().join("sub");
        let target = parent.join("data.json");

        std::fs::create_dir(&parent).expect("create parent");
        std::fs::write(&target, "v1").expect("write target");

        let (cmd_tx, mut event_rx, handle) =
            spawn_provider_watch_actor().expect("spawn real actor");

        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "lifecycle-p1".into(),
                files: vec![FileSubscription {
                    path: target.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Drain the declaration-to-install race event if any.
        tokio::time::timeout(Duration::from_millis(300), event_rx.recv())
            .await
            .ok();

        // Delete the parent directory (and thus the target).
        std::fs::remove_dir_all(&parent).expect("remove parent");

        // Wait for the debounced refresh (one-second debounce + margin).
        let event = tokio::time::timeout(Duration::from_secs(6), event_rx.recv())
            .await
            .expect("timeout waiting for first refresh after parent deletion")
            .expect("channel closed");

        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["lifecycle-p1".to_string()]);
            }
        }

        // Recreate parent + target.
        std::fs::create_dir(&parent).expect("recreate parent");
        std::fs::write(&target, "v2").expect("rewrite target");

        // Wait for the second debounced refresh.
        let event = tokio::time::timeout(Duration::from_secs(6), event_rx.recv())
            .await
            .expect("timeout waiting for second refresh after parent recreation")
            .expect("channel closed");

        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["lifecycle-p1".to_string()]);
            }
        }

        handle.abort();
    }

    // =====================================================================
    // Real notify: rapid parent move/delete and recreate before actor
    // processes events, then mutate target and assert event
    // =====================================================================

    #[tokio::test]
    async fn real_notify_rapid_parent_delete_recreate_stale_watch_regression() {
        // Register a target under an existing watched parent.  Rapidly
        // delete the parent and recreate it at the same path before the
        // actor processes the delete event.  Then independently mutate the
        // target file.  The test must fail if registry presence causes the
        // stale OS watch to be skipped during reinstallation.
        let dir = tempdir().expect("temp dir");
        let parent = dir.path().join("sub");
        let target = parent.join("data.json");

        std::fs::create_dir(&parent).expect("create parent");
        std::fs::write(&target, "v1").expect("write target");

        let (cmd_tx, mut event_rx, handle) =
            spawn_provider_watch_actor().expect("spawn real actor");

        let (ack, rx) = oneshot::channel();
        cmd_tx
            .send(ProviderWatchCommand::ReplaceProvider {
                provider_id: "stale-p1".into(),
                files: vec![FileSubscription {
                    path: target.clone(),
                    existed_at_declaration: true,
                }],
                ack,
            })
            .expect("send");
        rx.await.expect("ack");

        // Drain the declaration-to-install race event if any.
        tokio::time::timeout(Duration::from_millis(300), event_rx.recv())
            .await
            .ok();

        // Rapidly delete the parent directory and recreate it at the same
        // path, before the actor handles the remove event.  The actor's
        // debounce timer will fire after 3s, by which time both the delete
        // and the recreate have already happened.
        std::fs::remove_dir_all(&parent).expect("remove parent");
        // Immediately recreate the parent and target.
        std::fs::create_dir(&parent).expect("recreate parent");
        std::fs::write(&target, "v2").expect("rewrite target");

        // Wait for the debounced event (one-second debounce + margin).
        // The actor should detect the existence change and emit a refresh.
        let event = tokio::time::timeout(Duration::from_secs(6), event_rx.recv())
            .await
            .expect("timeout waiting for refresh after rapid delete/recreate")
            .expect("channel closed");

        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["stale-p1".to_string()]);
            }
        }

        // Now independently mutate the target file to prove the OS watch
        // was properly reinstalled after the recreate.
        std::fs::write(&target, "v3").expect("rewrite target v3");

        let event = tokio::time::timeout(Duration::from_secs(6), event_rx.recv())
            .await
            .expect("timeout waiting for refresh after independent mutation")
            .expect("channel closed");

        match event {
            ProviderWatchEvent::ProvidersChanged(ids) => {
                assert_eq!(ids, vec!["stale-p1".to_string()]);
            }
        }

        handle.abort();
    }
}
