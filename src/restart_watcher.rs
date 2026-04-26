use crate::http_api::LifecycleCommand;
use anyhow::{Context, Result};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};

const RESTART_DEBOUNCE_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct RestartWatchInputs {
    pub plugins_dir: PathBuf,
    pub plugin_overrides_dir: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub binary_file: PathBuf,
}

#[derive(Debug, Clone)]
struct WatchRoot {
    path: PathBuf,
    recursive: RecursiveMode,
}

#[derive(Debug, Clone, Default)]
struct WatchPlan {
    watch_roots: Vec<WatchRoot>,
    plugin_roots: Vec<PathBuf>,
    overrides_roots: Vec<PathBuf>,
    config_files: Vec<PathBuf>,
    binary_files: Vec<PathBuf>,
}

pub fn spawn_restart_watcher(
    inputs: RestartWatchInputs,
    lifecycle_tx: Arc<Mutex<Option<oneshot::Sender<LifecycleCommand>>>>,
) -> Result<tokio::task::JoinHandle<()>> {
    let watch_plan = build_watch_plan(inputs)?;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<notify::Result<Event>>();

    let mut watcher = RecommendedWatcher::new(
        move |event_result| {
            let _ = event_tx.send(event_result);
        },
        Config::default(),
    )
    .context("failed to initialize filesystem watcher backend")?;

    for watch_root in &watch_plan.watch_roots {
        watcher
            .watch(&watch_root.path, watch_root.recursive)
            .with_context(|| {
                format!(
                    "failed to watch path {} (recursive={:?})",
                    watch_root.path.display(),
                    watch_root.recursive
                )
            })?;
    }

    log::info!(
        "filesystem restart watcher enabled (debounce {}s, watch roots={})",
        RESTART_DEBOUNCE_DELAY.as_secs(),
        watch_plan.watch_roots.len()
    );

    let watcher_task = tokio::spawn(async move {
        let _watcher = watcher;
        let mut restart_deadline: Option<tokio::time::Instant> = None;

        loop {
            if let Some(deadline) = restart_deadline {
                tokio::select! {
                    maybe_event = event_rx.recv() => {
                        let Some(event_result) = maybe_event else {
                            log::warn!("filesystem watcher event channel closed unexpectedly");
                            break;
                        };

                        if handle_event(event_result, &watch_plan, &mut restart_deadline) {
                            continue;
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        log::info!(
                            "no new filesystem changes for {}s; triggering daemon restart",
                            RESTART_DEBOUNCE_DELAY.as_secs()
                        );

                        let mut tx_guard = lifecycle_tx.lock().await;
                        if let Some(tx) = tx_guard.take() {
                            if tx.send(LifecycleCommand::Restart).is_ok() {
                                log::info!("daemon restart requested by filesystem watcher");
                            } else {
                                log::warn!("filesystem watcher could not trigger restart: lifecycle channel already closed");
                            }
                        } else {
                            log::warn!("filesystem watcher could not trigger restart: lifecycle command already requested");
                        }

                        break;
                    }
                }
            } else {
                let Some(event_result) = event_rx.recv().await else {
                    log::warn!("filesystem watcher event channel closed unexpectedly");
                    break;
                };

                let _ = handle_event(event_result, &watch_plan, &mut restart_deadline);
            }
        }
    });

    Ok(watcher_task)
}

fn build_watch_plan(inputs: RestartWatchInputs) -> Result<WatchPlan> {
    let cwd = std::env::current_dir().context("cannot resolve current directory")?;
    let mut plan = WatchPlan::default();

    let plugins_dir = normalize_existing_path(&inputs.plugins_dir, &cwd);
    if !plugins_dir.is_dir() {
        anyhow::bail!(
            "plugins directory is not readable: {}",
            plugins_dir.display()
        );
    }
    add_watch_root(
        &mut plan.watch_roots,
        plugins_dir.clone(),
        RecursiveMode::Recursive,
    );
    plan.plugin_roots.push(plugins_dir);

    if let Some(overrides_dir_raw) = inputs.plugin_overrides_dir {
        let overrides_dir = normalize_existing_path(&overrides_dir_raw, &cwd);
        if !overrides_dir.is_dir() {
            anyhow::bail!(
                "plugin overrides directory is not readable: {}",
                overrides_dir.display()
            );
        }
        add_watch_root(
            &mut plan.watch_roots,
            overrides_dir.clone(),
            RecursiveMode::Recursive,
        );
        plan.overrides_roots.push(overrides_dir);
    }

    if let Some(config_file_raw) = inputs.config_file {
        let config_file = normalize_absolute_path(&config_file_raw, &cwd);
        add_file_watch(&mut plan.watch_roots, &config_file)?;
        plan.config_files = file_aliases(&config_file);
    }

    let binary_file = normalize_absolute_path(&inputs.binary_file, &cwd);
    add_file_watch(&mut plan.watch_roots, &binary_file)?;
    plan.binary_files = file_aliases(&binary_file);

    Ok(plan)
}

fn handle_event(
    event_result: notify::Result<Event>,
    watch_plan: &WatchPlan,
    restart_deadline: &mut Option<tokio::time::Instant>,
) -> bool {
    let event = match event_result {
        Ok(event) => event,
        Err(err) => {
            log::warn!("filesystem watcher backend error: {}", err);
            return false;
        }
    };

    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }

    let Some(change_description) = event
        .paths
        .iter()
        .find_map(|path| classify_changed_path(path, watch_plan))
    else {
        return false;
    };

    if restart_deadline.is_some() {
        log::info!(
            "detected additional filesystem change ({change_description}); restarting debounce timer ({}s)",
            RESTART_DEBOUNCE_DELAY.as_secs()
        );
    } else {
        log::info!(
            "detected filesystem change ({change_description}); scheduling daemon restart in {}s",
            RESTART_DEBOUNCE_DELAY.as_secs()
        );
    }

    *restart_deadline = Some(tokio::time::Instant::now() + RESTART_DEBOUNCE_DELAY);
    true
}

fn classify_changed_path(path: &Path, watch_plan: &WatchPlan) -> Option<String> {
    let normalized = normalize_event_path(path);

    if watch_plan
        .plugin_roots
        .iter()
        .any(|plugins_root| normalized.starts_with(plugins_root))
    {
        return Some(format!("plugin file: {}", normalized.display()));
    }

    if watch_plan
        .overrides_roots
        .iter()
        .any(|overrides_root| normalized.starts_with(overrides_root))
    {
        return Some(format!("plugin override file: {}", normalized.display()));
    }

    if path_matches_aliases(&normalized, &watch_plan.config_files) {
        return Some(format!("config file: {}", normalized.display()));
    }

    if path_matches_aliases(&normalized, &watch_plan.binary_files) {
        return Some(format!("binary file: {}", normalized.display()));
    }

    None
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

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, ModifyKind};
    use tempfile::tempdir;

    fn event_with_path(kind: EventKind, path: &Path) -> Event {
        Event::new(kind).add_path(path.to_path_buf())
    }

    fn watch_mode_for(plan: &WatchPlan, path: &Path) -> Option<RecursiveMode> {
        plan.watch_roots
            .iter()
            .find(|root| root.path == path)
            .map(|root| root.recursive)
    }

    #[test]
    fn classify_changed_path_detects_plugins_and_overrides() {
        let plan = WatchPlan {
            plugin_roots: vec![PathBuf::from("/tmp/plugins")],
            overrides_roots: vec![PathBuf::from("/tmp/plugin-overrides")],
            ..WatchPlan::default()
        };

        let plugin_change = classify_changed_path(Path::new("/tmp/plugins/codex/plugin.js"), &plan)
            .expect("plugin change should be detected");
        assert!(plugin_change.contains("plugin file"));

        let override_change =
            classify_changed_path(Path::new("/tmp/plugin-overrides/codex.js"), &plan)
                .expect("override change should be detected");
        assert!(override_change.contains("plugin override file"));
    }

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

    #[test]
    fn classify_changed_path_detects_config_and_binary_files() {
        let plan = WatchPlan {
            config_files: vec![PathBuf::from("/tmp/openusage/config.yaml")],
            binary_files: vec![PathBuf::from("/tmp/openusage/openusage-cli")],
            ..WatchPlan::default()
        };

        let config_change = classify_changed_path(Path::new("/tmp/openusage/config.yaml"), &plan)
            .expect("config change should be detected");
        assert!(config_change.contains("config file"));

        let binary_change = classify_changed_path(Path::new("/tmp/openusage/openusage-cli"), &plan)
            .expect("binary change should be detected");
        assert!(binary_change.contains("binary file"));

        assert!(classify_changed_path(Path::new("/tmp/openusage/other.txt"), &plan).is_none());
    }

    #[test]
    fn handle_event_ignores_access_events() {
        let plan = WatchPlan {
            plugin_roots: vec![PathBuf::from("/tmp/plugins")],
            ..WatchPlan::default()
        };
        let mut restart_deadline = None;
        let event = event_with_path(
            EventKind::Access(AccessKind::Any),
            Path::new("/tmp/plugins/codex/plugin.js"),
        );

        let changed = handle_event(Ok(event), &plan, &mut restart_deadline);

        assert!(!changed);
        assert!(restart_deadline.is_none());
    }

    #[test]
    fn handle_event_resets_debounce_deadline_after_new_changes() {
        let temp = tempdir().expect("temp dir");
        let plugins_dir = temp.path().join("plugins");
        std::fs::create_dir_all(&plugins_dir).expect("create plugins dir");

        let plan = WatchPlan {
            plugin_roots: vec![plugins_dir.clone()],
            ..WatchPlan::default()
        };
        let mut restart_deadline = None;

        let first_event = event_with_path(
            EventKind::Create(CreateKind::Any),
            &plugins_dir.join("codex.js"),
        );
        assert!(handle_event(Ok(first_event), &plan, &mut restart_deadline));
        let first_deadline = restart_deadline.expect("deadline after first event");

        std::thread::sleep(Duration::from_millis(10));

        let second_event = event_with_path(
            EventKind::Modify(ModifyKind::Any),
            &plugins_dir.join("cursor.js"),
        );
        assert!(handle_event(Ok(second_event), &plan, &mut restart_deadline));
        let second_deadline = restart_deadline.expect("deadline after second event");

        assert!(second_deadline > first_deadline);
    }

    #[test]
    fn build_watch_plan_tracks_plugins_overrides_config_and_binary() {
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

        let plan = build_watch_plan(RestartWatchInputs {
            plugins_dir: plugins_dir.clone(),
            plugin_overrides_dir: Some(overrides_dir.clone()),
            config_file: Some(config_file.clone()),
            binary_file: binary_file.clone(),
        })
        .expect("build watch plan");

        assert_eq!(plan.plugin_roots, vec![plugins_dir]);
        assert_eq!(plan.overrides_roots, vec![overrides_dir]);
        assert!(plan.config_files.contains(&config_file));
        assert!(plan.binary_files.contains(&binary_file));
        assert_eq!(
            watch_mode_for(&plan, config_file.parent().expect("config parent")),
            Some(RecursiveMode::NonRecursive)
        );
        assert_eq!(
            watch_mode_for(&plan, binary_file.parent().expect("binary parent")),
            Some(RecursiveMode::NonRecursive)
        );
    }
}
