use super::*;
use directories::ProjectDirs;
use std::path::Path;

pub(super) fn plugins_dir_candidates(cwd: &Path, exec_dir: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let source_root = source_checkout_root_from_exec_dir(exec_dir);

    if let Some(source_root) = source_root {
        push_unique_path(
            &mut candidates,
            source_root.join("vendor/openusage/plugins"),
        );
        push_unique_path(&mut candidates, source_root.join("plugins"));
        push_unique_path(&mut candidates, cwd.join("vendor/openusage/plugins"));
        push_unique_path(&mut candidates, cwd.join("plugins"));
        push_unique_path(&mut candidates, exec_dir.join("vendor/openusage/plugins"));
        push_unique_path(&mut candidates, exec_dir.join("plugins"));
    }

    if let Some(packaged_path) = packaged_plugins_dir_from_exec_dir(exec_dir) {
        push_unique_path(&mut candidates, packaged_path);
    }
    push_unique_path(&mut candidates, PathBuf::from(SYSTEM_PLUGINS_DIR));

    candidates
}

pub(super) fn plugin_overrides_dir_candidates(cwd: &Path, exec_dir: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let source_root = source_checkout_root_from_exec_dir(exec_dir);

    if let Some(source_root) = source_root {
        push_unique_path(&mut candidates, source_root.join("plugin-overrides"));
        push_unique_path(&mut candidates, cwd.join("plugin-overrides"));
        push_unique_path(&mut candidates, exec_dir.join("plugin-overrides"));
    }

    if let Some(packaged_path) = packaged_overrides_dir_from_exec_dir(exec_dir) {
        push_unique_path(&mut candidates, packaged_path);
    }
    push_unique_path(&mut candidates, PathBuf::from(SYSTEM_PLUGIN_OVERRIDES_DIR));

    candidates
}

pub(super) fn source_checkout_root_from_exec_dir(exec_dir: &Path) -> Option<PathBuf> {
    let profile = exec_dir.file_name()?.to_str()?;
    if profile != "debug" && profile != "release" {
        return None;
    }

    let parent = exec_dir.parent()?;
    if parent.file_name().and_then(|name| name.to_str()) == Some("target") {
        return parent.parent().map(Path::to_path_buf);
    }

    let maybe_target = parent.parent()?;
    if maybe_target.file_name().and_then(|name| name.to_str()) == Some("target") {
        return maybe_target.parent().map(Path::to_path_buf);
    }

    None
}

pub(super) fn packaged_plugins_dir_from_exec_dir(exec_dir: &Path) -> Option<PathBuf> {
    exec_dir
        .parent()
        .map(|prefix| prefix.join("share/openusage-cli/openusage-plugins"))
}

pub(super) fn packaged_overrides_dir_from_exec_dir(exec_dir: &Path) -> Option<PathBuf> {
    exec_dir
        .parent()
        .map(|prefix| prefix.join("share/openusage-cli/plugin-overrides"))
}

pub(super) fn push_unique_path(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

pub(super) fn resolve_app_data_dir(cli_value: Option<PathBuf>, test_mode: bool) -> Result<PathBuf> {
    if let Some(path) = cli_value {
        log::debug!("app data dir provided via CLI/env: {}", path.display());
        return Ok(path);
    }
    if test_mode {
        anyhow::bail!("--app-data-dir is required when --test-mode is enabled");
    }
    if let Some(project_dirs) = ProjectDirs::from("com", "openusage", "openusage-cli") {
        let resolved = project_dirs.data_local_dir().to_path_buf();
        log::debug!(
            "app data dir resolved via ProjectDirs: {}",
            resolved.display()
        );
        return Ok(resolved);
    }
    let cwd = std::env::current_dir().context("cannot get current directory")?;
    let fallback = cwd.join(".openusage-cli");
    log::debug!(
        "app data dir fallback to current dir: {}",
        fallback.display()
    );
    Ok(fallback)
}

pub(super) fn resolve_plugins_dir(cli_value: Option<PathBuf>, test_mode: bool) -> Result<PathBuf> {
    if let Some(path) = cli_value {
        log::debug!("plugins dir provided via CLI/env: {}", path.display());
        return Ok(path);
    }
    if test_mode {
        anyhow::bail!("--plugins-dir is required when --test-mode is enabled");
    }

    let cwd = std::env::current_dir().context("cannot get current directory")?;
    let exec_dir = executable_dir()?;
    let candidates = plugins_dir_candidates(&cwd, &exec_dir);

    for candidate in candidates {
        log::debug!("checking plugins dir candidate {}", candidate.display());
        if candidate.is_dir() {
            log::debug!("plugins dir candidate selected: {}", candidate.display());
            return Ok(candidate);
        }
    }

    anyhow::bail!("plugins directory not found")
}

pub(super) fn resolve_plugin_overrides_dir(
    cli_value: Option<PathBuf>,
    test_mode: bool,
) -> Result<Option<PathBuf>> {
    if let Some(path) = cli_value {
        if !path.exists() {
            anyhow::bail!("plugin overrides dir does not exist: {}", path.display());
        }
        if !path.is_dir() {
            anyhow::bail!(
                "plugin overrides path is not a directory: {}",
                path.display()
            );
        }
        log::debug!(
            "plugin overrides dir provided via CLI/env: {}",
            path.display()
        );
        return Ok(Some(path));
    }

    if test_mode {
        return Ok(None);
    }

    let cwd = std::env::current_dir().context("cannot get current directory")?;
    let exec_dir = executable_dir()?;
    let candidates = plugin_overrides_dir_candidates(&cwd, &exec_dir);

    for candidate in candidates {
        log::debug!(
            "checking plugin overrides dir candidate {}",
            candidate.display()
        );
        if candidate.is_dir() {
            log::debug!(
                "plugin overrides dir candidate selected: {}",
                candidate.display()
            );
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

pub(super) fn executable_dir() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("cannot resolve executable path")?;
    let dir = executable
        .parent()
        .map(Path::to_path_buf)
        .context("executable has no parent directory")?;
    Ok(dir)
}
