use crate::plugin_engine::manifest::{LoadedPlugin, ManifestLine, PluginLink};
use crate::plugin_engine::runtime::{self, MetricLine};
use crate::restart_watcher::ProviderWatchCommand;
use anyhow::anyhow;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestLineDto {
    #[serde(rename = "type")]
    pub line_type: String,
    pub label: String,
    pub scope: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginLinkDto {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMeta {
    pub id: String,
    pub name: String,
    pub icon_url: String,
    pub brand_color: Option<String>,
    pub lines: Vec<ManifestLineDto>,
    pub links: Vec<PluginLinkDto>,
    pub primary_candidates: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedPluginSnapshot {
    pub provider_id: String,
    pub display_name: String,
    pub plan: Option<String>,
    pub lines: Vec<MetricLine>,
    pub fetched_at: String,
}

impl CachedPluginSnapshot {
    fn from_output(output: runtime::PluginOutput) -> Self {
        Self {
            provider_id: output.provider_id,
            display_name: output.display_name,
            plan: output.plan,
            lines: output.lines,
            fetched_at: now_iso(),
        }
    }
}

pub struct DaemonState {
    plugins: Vec<LoadedPlugin>,
    app_data_dir: PathBuf,
    app_version: String,
    plugin_overrides_dir: Option<PathBuf>,
    cache: RwLock<HashMap<String, CachedPluginSnapshot>>,
    refresh_lock: Mutex<()>,
    /// Optional command sender for the provider file-monitor actor.
    /// When set, ClearProvider/ReplaceProvider are sent around each probe.
    monitor_cmd_tx: Option<mpsc::UnboundedSender<ProviderWatchCommand>>,
}

impl DaemonState {
    pub fn new(
        plugins: Vec<LoadedPlugin>,
        app_data_dir: PathBuf,
        app_version: String,
        plugin_overrides_dir: Option<PathBuf>,
        monitor_cmd_tx: Option<mpsc::UnboundedSender<ProviderWatchCommand>>,
    ) -> Self {
        log::debug!(
            "initializing daemon state: plugins={}, app_data_dir={}, app_version={}, plugin_overrides_dir={}",
            plugins.len(),
            app_data_dir.display(),
            app_version,
            plugin_overrides_dir
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<none>".to_string())
        );
        Self {
            plugins,
            app_data_dir,
            app_version,
            plugin_overrides_dir,
            cache: RwLock::new(HashMap::new()),
            refresh_lock: Mutex::new(()),
            monitor_cmd_tx,
        }
    }

    pub fn plugins_meta(&self) -> Vec<PluginMeta> {
        self.plugins
            .iter()
            .map(|plugin| PluginMeta {
                id: plugin.manifest.id.clone(),
                name: plugin.manifest.name.clone(),
                icon_url: plugin.icon_data_url.clone(),
                brand_color: plugin.manifest.brand_color.clone(),
                lines: plugin
                    .manifest
                    .lines
                    .iter()
                    .map(map_line)
                    .collect::<Vec<ManifestLineDto>>(),
                links: plugin
                    .manifest
                    .links
                    .iter()
                    .map(map_link)
                    .collect::<Vec<PluginLinkDto>>(),
                primary_candidates: primary_candidates(&plugin.manifest.lines),
            })
            .collect()
    }

    pub fn has_plugin(&self, plugin_id: &str) -> bool {
        self.plugins.iter().any(|p| p.manifest.id == plugin_id)
    }

    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }

    pub async fn cached(&self, plugin_ids: Option<&[String]>) -> Vec<CachedPluginSnapshot> {
        let cache = self.cache.read().await;
        let selected_ids = self.resolve_plugin_ids(plugin_ids);
        selected_ids
            .iter()
            .filter_map(|id| cache.get(id).cloned())
            .collect()
    }

    pub async fn cached_one(&self, plugin_id: &str) -> Option<CachedPluginSnapshot> {
        let cache = self.cache.read().await;
        cache.get(plugin_id).cloned()
    }

    pub async fn refresh(
        &self,
        plugin_ids: Option<Vec<String>>,
    ) -> anyhow::Result<Vec<CachedPluginSnapshot>> {
        let requested_ids = plugin_ids.clone();
        log::debug!("refresh requested for plugin_ids={:?}", requested_ids);

        let _lock = self.refresh_lock.lock().await;
        let selected = self.resolve_plugins(plugin_ids.as_deref());
        let selected_ids: Vec<&str> = selected.iter().map(|p| p.manifest.id.as_str()).collect();
        log::debug!(
            "refresh started: selected_plugins={} {:?}",
            selected.len(),
            selected_ids
        );

        let mut snapshots = Vec::with_capacity(selected.len());

        for plugin in selected {
            let plugin_id = plugin.manifest.id.clone();
            let data_dir = self.app_data_dir.clone();
            let app_version = self.app_version.clone();
            let plugin_overrides_dir = self.plugin_overrides_dir.clone();

            // --- Provider file-monitor: clear subscriptions before probe ---
            if let Some(cmd_tx) = &self.monitor_cmd_tx {
                let (ack, rx) = oneshot::channel();
                if cmd_tx
                    .send(ProviderWatchCommand::ClearProvider {
                        provider_id: plugin_id.clone(),
                        ack,
                    })
                    .is_err()
                {
                    log::warn!(
                        "[daemon] monitor channel closed before clear for {}",
                        plugin_id
                    );
                } else {
                    if rx.await.is_err() {
                        log::warn!(
                            "[daemon] {} acknowledgement canceled for {}",
                            "clear",
                            plugin_id
                        );
                    }
                }
            }

            log::debug!("running probe for plugin {}", plugin_id);
            let probe_result = tokio::task::spawn_blocking(move || {
                runtime::run_probe(
                    &plugin,
                    &data_dir,
                    &app_version,
                    plugin_overrides_dir.as_deref(),
                )
            })
            .await
            .map_err(|err| anyhow!("plugin probe join error: {err}"))?;

            log::debug!(
                "probe finished for plugin {}: lines={}, subscriptions={}",
                probe_result.output.provider_id,
                probe_result.output.lines.len(),
                probe_result.subscriptions.len(),
            );

            // --- Provider file-monitor: replace subscriptions after probe ---
            if let Some(cmd_tx) = &self.monitor_cmd_tx {
                let (ack, rx) = oneshot::channel();
                if cmd_tx
                    .send(ProviderWatchCommand::ReplaceProvider {
                        provider_id: probe_result.output.provider_id.clone(),
                        files: probe_result.subscriptions.clone(),
                        ack,
                    })
                    .is_err()
                {
                    log::warn!(
                        "[daemon] monitor channel closed before replace for {}",
                        probe_result.output.provider_id
                    );
                } else {
                    if rx.await.is_err() {
                        log::warn!(
                            "[daemon] {} acknowledgement canceled for {}",
                            "replace",
                            probe_result.output.provider_id
                        );
                    }
                }
            }

            snapshots.push(CachedPluginSnapshot::from_output(probe_result.output));
        }

        let mut cache = self.cache.write().await;
        for snapshot in &snapshots {
            cache.insert(snapshot.provider_id.clone(), snapshot.clone());
        }

        log::debug!(
            "refresh finished: updated_snapshots={}, cache_size={}",
            snapshots.len(),
            cache.len()
        );

        Ok(snapshots)
    }

    pub async fn has_cached_for(&self, plugin_ids: Option<&[String]>) -> bool {
        let cache = self.cache.read().await;
        let selected_ids = self.resolve_plugin_ids(plugin_ids);
        selected_ids.iter().all(|id| cache.contains_key(id))
    }

    /// Calculates the duration until the next limit reset across all cached snapshots.
    /// Returns None if no resets are scheduled or if all resets are in the past.
    /// The `margin_secs` parameter adds a buffer after the reset time to ensure
    /// the provider has actually updated their data.
    pub async fn time_until_next_reset(&self, margin_secs: u64) -> Option<Duration> {
        self.next_reset_with_delay(margin_secs)
            .await
            .map(|(_, delay)| delay)
    }

    /// Returns the earliest future reset marker and its effective delay.
    ///
    /// The first tuple item is the original `resetsAt` value from provider data.
    /// The second item is the duration until `resetsAt + margin_secs`.
    pub async fn next_reset_with_delay(&self, margin_secs: u64) -> Option<(String, Duration)> {
        let cache = self.cache.read().await;
        let now = time::OffsetDateTime::now_utc();
        let mut next_reset: Option<(time::OffsetDateTime, String)> = None;

        for snapshot in cache.values() {
            for line in &snapshot.lines {
                if let MetricLine::Progress {
                    resets_at: Some(resets_at_str),
                    ..
                } = line
                    && let Ok(reset_time) = time::OffsetDateTime::parse(
                        resets_at_str,
                        &time::format_description::well_known::Rfc3339,
                    )
                {
                    // Add margin to the reset time
                    let effective_reset = reset_time + time::Duration::seconds(margin_secs as i64);
                    if effective_reset > now {
                        let should_update = match &next_reset {
                            None => true,
                            Some((earliest, _)) => effective_reset < *earliest,
                        };
                        if should_update {
                            next_reset = Some((effective_reset, resets_at_str.clone()));
                        }
                    }
                }
            }
        }

        next_reset.map(|(reset_time, resets_at)| {
            let duration_ms = (reset_time - now).whole_milliseconds().max(0) as u64;
            (resets_at, Duration::from_millis(duration_ms))
        })
    }

    /// Checks if there are any reset times that are in the past (plus margin).
    /// This indicates that a limit was supposed to reset but the provider data
    /// hasn't been updated yet. Returns true if at least one past reset is found.
    pub async fn has_past_resets(&self, margin_secs: u64) -> bool {
        !self
            .provider_ids_with_past_resets(margin_secs)
            .await
            .is_empty()
    }

    /// Returns provider ids that still have at least one reset time in the past
    /// (after applying margin). Output is sorted for stable logging and tests.
    pub async fn provider_ids_with_past_resets(&self, margin_secs: u64) -> Vec<String> {
        let cache = self.cache.read().await;
        let now = time::OffsetDateTime::now_utc();
        let mut provider_ids = Vec::new();

        for snapshot in cache.values() {
            let mut has_past_reset = false;
            for line in &snapshot.lines {
                if let MetricLine::Progress {
                    resets_at: Some(resets_at_str),
                    ..
                } = line
                    && let Ok(reset_time) = time::OffsetDateTime::parse(
                        resets_at_str,
                        &time::format_description::well_known::Rfc3339,
                    )
                {
                    let effective_reset = reset_time + time::Duration::seconds(margin_secs as i64);
                    if effective_reset <= now {
                        has_past_reset = true;
                        break;
                    }
                }
            }

            if has_past_reset {
                provider_ids.push(snapshot.provider_id.clone());
            }
        }

        provider_ids.sort_unstable();
        provider_ids
    }

    fn resolve_plugins(&self, plugin_ids: Option<&[String]>) -> Vec<LoadedPlugin> {
        let Some(plugin_ids) = plugin_ids else {
            return self.plugins.clone();
        };

        let set: HashSet<&str> = plugin_ids.iter().map(String::as_str).collect();
        self.plugins
            .iter()
            .filter(|p| set.contains(p.manifest.id.as_str()))
            .cloned()
            .collect()
    }

    fn resolve_plugin_ids(&self, plugin_ids: Option<&[String]>) -> Vec<String> {
        let Some(plugin_ids) = plugin_ids else {
            return self.plugins.iter().map(|p| p.manifest.id.clone()).collect();
        };

        let mut seen = HashSet::new();
        plugin_ids
            .iter()
            .filter_map(|id| {
                if !self.has_plugin(id) {
                    return None;
                }
                if seen.insert(id.clone()) {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    }
}

fn map_line(line: &ManifestLine) -> ManifestLineDto {
    ManifestLineDto {
        line_type: line.line_type.clone(),
        label: line.label.clone(),
        scope: line.scope.clone(),
    }
}

fn map_link(link: &PluginLink) -> PluginLinkDto {
    PluginLinkDto {
        label: link.label.clone(),
        url: link.url.clone(),
    }
}

fn primary_candidates(lines: &[ManifestLine]) -> Vec<String> {
    let mut candidates: Vec<&ManifestLine> = lines
        .iter()
        .filter(|line| line.line_type == "progress" && line.primary_order.is_some())
        .collect();
    candidates.sort_by_key(|line| line.primary_order.unwrap_or(u32::MAX));
    candidates
        .into_iter()
        .map(|line| line.label.clone())
        .collect()
}

fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_engine::manifest::{LoadedPlugin, PluginManifest};
    use crate::plugin_engine::runtime::ProgressFormat;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn test_plugin(entry_script: &str) -> LoadedPlugin {
        LoadedPlugin {
            manifest: PluginManifest {
                schema_version: 1,
                id: "test".to_string(),
                name: "Test".to_string(),
                version: "0.0.0".to_string(),
                entry: "plugin.js".to_string(),
                icon: "icon.svg".to_string(),
                brand_color: None,
                lines: vec![],
                links: vec![],
            },
            plugin_dir: PathBuf::from("."),
            entry_script: entry_script.to_string(),
            icon_data_url: "data:image/svg+xml;base64,".to_string(),
        }
    }

    fn iso_from_now(offset_secs: i64) -> String {
        (time::OffsetDateTime::now_utc() + time::Duration::seconds(offset_secs))
            .format(&time::format_description::well_known::Rfc3339)
            .expect("rfc3339 timestamp")
    }

    fn progress_line(resets_at: Option<String>) -> MetricLine {
        MetricLine::Progress {
            label: "Limit".to_string(),
            used: 10.0,
            limit: 100.0,
            format: ProgressFormat::Percent,
            resets_at,
            period_duration_ms: None,
            color: None,
        }
    }

    fn text_line() -> MetricLine {
        MetricLine::Text {
            label: "Info".to_string(),
            value: "ok".to_string(),
            color: None,
            subtitle: None,
        }
    }

    fn snapshot(provider_id: &str, lines: Vec<MetricLine>) -> CachedPluginSnapshot {
        CachedPluginSnapshot {
            provider_id: provider_id.to_string(),
            display_name: provider_id.to_string(),
            plan: None,
            lines,
            fetched_at: now_iso(),
        }
    }

    async fn state_with_cache(snapshots: Vec<CachedPluginSnapshot>) -> DaemonState {
        let state = DaemonState::new(
            Vec::new(),
            PathBuf::from("."),
            "0.0.0-test".to_string(),
            None,
            None,
        );
        let mut cache = state.cache.write().await;
        for item in snapshots {
            cache.insert(item.provider_id.clone(), item);
        }
        drop(cache);
        state
    }

    #[tokio::test]
    async fn reset_helpers_return_none_for_empty_cache() {
        let state = state_with_cache(Vec::new()).await;

        assert_eq!(state.time_until_next_reset(5).await, None);
        assert!(!state.has_past_resets(5).await);
    }

    #[tokio::test]
    async fn time_until_next_reset_picks_earliest_future_with_margin() {
        let state = state_with_cache(vec![
            snapshot("later", vec![progress_line(Some(iso_from_now(120)))]),
            snapshot(
                "earlier",
                vec![
                    progress_line(Some(iso_from_now(25))),
                    progress_line(Some("not-a-date".to_string())),
                ],
            ),
        ])
        .await;

        let delay = state
            .time_until_next_reset(5)
            .await
            .expect("next reset should exist");

        assert!(
            delay >= Duration::from_secs(27) && delay <= Duration::from_secs(31),
            "unexpected delay: {:?}",
            delay
        );
    }

    #[tokio::test]
    async fn next_reset_with_delay_returns_earliest_raw_resets_at() {
        let earliest = iso_from_now(25);
        let state = state_with_cache(vec![
            snapshot("later", vec![progress_line(Some(iso_from_now(120)))]),
            snapshot("earlier", vec![progress_line(Some(earliest.clone()))]),
        ])
        .await;

        let (resets_at, delay) = state
            .next_reset_with_delay(5)
            .await
            .expect("next reset should exist");

        assert_eq!(resets_at, earliest);
        assert!(
            delay >= Duration::from_secs(27) && delay <= Duration::from_secs(31),
            "unexpected delay: {:?}",
            delay
        );
    }

    #[tokio::test]
    async fn has_past_resets_true_when_any_effective_reset_is_past() {
        let state = state_with_cache(vec![
            snapshot("past", vec![progress_line(Some(iso_from_now(-20)))]),
            snapshot("future", vec![progress_line(Some(iso_from_now(50)))]),
        ])
        .await;

        assert!(state.has_past_resets(5).await);
    }

    #[tokio::test]
    async fn provider_ids_with_past_resets_returns_only_stale_ids_sorted() {
        let state = state_with_cache(vec![
            snapshot("future", vec![progress_line(Some(iso_from_now(120)))]),
            snapshot("past-b", vec![progress_line(Some(iso_from_now(-20)))]),
            snapshot("past-a", vec![progress_line(Some(iso_from_now(-40)))]),
            snapshot(
                "invalid",
                vec![
                    text_line(),
                    progress_line(None),
                    progress_line(Some("bad-timestamp".to_string())),
                ],
            ),
        ])
        .await;

        assert_eq!(
            state.provider_ids_with_past_resets(5).await,
            vec!["past-a".to_string(), "past-b".to_string()]
        );
    }

    #[tokio::test]
    async fn margin_can_prevent_recent_reset_from_being_considered_past() {
        let state = state_with_cache(vec![snapshot(
            "recently-past",
            vec![progress_line(Some(iso_from_now(-2)))],
        )])
        .await;

        let delay = state
            .time_until_next_reset(5)
            .await
            .expect("effective reset should still be in the future");

        assert!(
            delay <= Duration::from_secs(4),
            "unexpected delay: {:?}",
            delay
        );
        assert!(!state.has_past_resets(5).await);
    }

    #[tokio::test]
    async fn reset_helpers_ignore_non_progress_and_invalid_lines() {
        let state = state_with_cache(vec![snapshot(
            "mixed",
            vec![
                text_line(),
                progress_line(None),
                progress_line(Some("bad-timestamp".to_string())),
            ],
        )])
        .await;

        assert_eq!(state.time_until_next_reset(5).await, None);
        assert!(!state.has_past_resets(5).await);
    }

    #[tokio::test]
    async fn time_until_next_reset_ignores_only_past_resets() {
        let state = state_with_cache(vec![
            snapshot("past-a", vec![progress_line(Some(iso_from_now(-120)))]),
            snapshot("past-b", vec![progress_line(Some(iso_from_now(-15)))]),
        ])
        .await;

        assert_eq!(state.time_until_next_reset(5).await, None);
        assert!(state.has_past_resets(5).await);
    }

    #[tokio::test]
    async fn refresh_sends_clear_before_probe_and_replace_after() {
        // A minimal plugin whose probe calls subscribeFile on a known path.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    ctx.host.fs.subscribeFile("/tmp/test-dep.json");
                    return {
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let app_data = std::env::temp_dir().join("daemon-ctrl-test");
        std::fs::create_dir_all(&app_data).ok();

        let state = Arc::new(DaemonState::new(
            vec![plugin],
            app_data,
            "0.0.0-test".to_string(),
            None,
            Some(cmd_tx),
        ));

        // Spawn refresh in background; it will block on ClearProvider ack.
        let state_clone = Arc::clone(&state);
        let refresh_handle = tokio::spawn(async move { state_clone.refresh(None).await });

        // 1) Assert ClearProvider is sent before probe proceeds.
        let clear_cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
            .await
            .expect("timeout waiting for ClearProvider")
            .expect("channel closed before ClearProvider");

        match &clear_cmd {
            ProviderWatchCommand::ClearProvider {
                provider_id,
                ack: _,
            } => {
                assert_eq!(
                    provider_id, "test",
                    "ClearProvider should target test plugin"
                );
            }
            other => panic!("expected ClearProvider, got {:?}", other),
        }

        // At this point refresh is blocked waiting for the ack. No ReplaceProvider yet.
        let replace_check = tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv()).await;
        assert!(
            replace_check.is_err(),
            "ReplaceProvider should not arrive before ClearProvider ack"
        );

        // 2) Acknowledge ClearProvider so probe can run.
        if let ProviderWatchCommand::ClearProvider { ack, .. } = clear_cmd {
            let _ = ack.send(());
        }

        // 3) Assert ReplaceProvider arrives with the subscribed file.
        let replace_cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
            .await
            .expect("timeout waiting for ReplaceProvider")
            .expect("channel closed before ReplaceProvider");

        match &replace_cmd {
            ProviderWatchCommand::ReplaceProvider {
                provider_id,
                files,
                ack: _,
            } => {
                assert_eq!(
                    provider_id, "test",
                    "ReplaceProvider should target test plugin"
                );
                assert_eq!(files.len(), 1, "expected 1 subscribed file");
                assert!(
                    files[0].path.to_string_lossy().ends_with("test-dep.json"),
                    "expected test-dep.json, got {:?}",
                    files[0]
                );
            }
            other => panic!("expected ReplaceProvider, got {:?}", other),
        }

        // 4) Acknowledge ReplaceProvider so refresh completes.
        if let ProviderWatchCommand::ReplaceProvider { ack, .. } = replace_cmd {
            let _ = ack.send(());
        }

        // 5) Assert refresh completed successfully and cached the snapshot.
        let snapshots = refresh_handle
            .await
            .expect("refresh task panicked")
            .expect("refresh failed");

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].provider_id, "test");
        assert_eq!(snapshots[0].lines.len(), 1);

        // Verify cache was written.
        let cached = state.cached_one("test").await;
        assert!(cached.is_some(), "snapshot should be cached");
    }

    #[tokio::test]
    async fn refresh_sends_replace_with_empty_files_when_no_subscriptions() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let app_data = std::env::temp_dir().join("daemon-ctrl-empty-test");
        std::fs::create_dir_all(&app_data).ok();

        let state = Arc::new(DaemonState::new(
            vec![plugin],
            app_data,
            "0.0.0-test".to_string(),
            None,
            Some(cmd_tx),
        ));

        let state_clone = Arc::clone(&state);
        let refresh_handle = tokio::spawn(async move { state_clone.refresh(None).await });

        // ClearProvider
        let clear_cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
            .await
            .expect("timeout waiting for ClearProvider")
            .expect("channel closed");
        if let ProviderWatchCommand::ClearProvider { ack, .. } = clear_cmd {
            let _ = ack.send(());
        }

        // ReplaceProvider with empty files
        let replace_cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
            .await
            .expect("timeout waiting for ReplaceProvider")
            .expect("channel closed");

        match &replace_cmd {
            ProviderWatchCommand::ReplaceProvider {
                provider_id,
                files,
                ack: _,
            } => {
                assert_eq!(provider_id, "test");
                assert!(
                    files.is_empty(),
                    "expected empty files when probe calls no subscribeFile"
                );
            }
            other => panic!("expected ReplaceProvider, got {:?}", other),
        }

        if let ProviderWatchCommand::ReplaceProvider { ack, .. } = replace_cmd {
            let _ = ack.send(());
        }

        let snapshots = refresh_handle
            .await
            .expect("refresh task panicked")
            .expect("refresh failed");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].provider_id, "test");
    }

    #[tokio::test]
    async fn refresh_replace_provider_carries_existence_metadata() {
        // Use a temp directory with one explicitly created file and one
        // missing sibling.  Assert both true and false metadata values
        // in the ReplaceProvider command.
        let tmp = tempfile::tempdir().expect("temp dir");
        let existing_path = tmp.path().join("present.json");
        std::fs::write(&existing_path, "data").expect("write");
        let missing_path = tmp.path().join("absent.json");

        let plugin = test_plugin(&format!(
            r#"
            globalThis.__openusage_plugin = {{
                probe(ctx) {{
                    ctx.host.fs.subscribeFile("{}");
                    ctx.host.fs.subscribeFile("{}");
                    return {{
                        lines: [ctx.line.text({{ label: "Status", value: "ok" }})]
                    }};
                }}
            }};
            "#,
            existing_path.to_string_lossy().replace("\\", "\\\\"),
            missing_path.to_string_lossy().replace("\\", "\\\\"),
        ));

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let app_data = std::env::temp_dir().join("daemon-existence-test");
        std::fs::create_dir_all(&app_data).ok();

        let state = Arc::new(DaemonState::new(
            vec![plugin],
            app_data,
            "0.0.0-test".to_string(),
            None,
            Some(cmd_tx),
        ));

        let state_clone = Arc::clone(&state);
        let refresh_handle = tokio::spawn(async move { state_clone.refresh(None).await });

        // ClearProvider
        let clear_cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
            .await
            .expect("timeout waiting for ClearProvider")
            .expect("channel closed");
        if let ProviderWatchCommand::ClearProvider { ack, .. } = clear_cmd {
            let _ = ack.send(());
        }

        // ReplaceProvider
        let replace_cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
            .await
            .expect("timeout waiting for ReplaceProvider")
            .expect("channel closed");

        match &replace_cmd {
            ProviderWatchCommand::ReplaceProvider {
                provider_id,
                files,
                ack: _,
            } => {
                assert_eq!(provider_id, "test");
                assert_eq!(files.len(), 2, "expected 2 subscribed files");

                let existing_sub = files
                    .iter()
                    .find(|f| f.path == existing_path)
                    .expect("expected present.json subscription");
                assert!(
                    existing_sub.existed_at_declaration,
                    "present.json should have existed_at_declaration=true"
                );

                let missing_sub = files
                    .iter()
                    .find(|f| f.path == missing_path)
                    .expect("expected absent.json subscription");
                assert!(
                    !missing_sub.existed_at_declaration,
                    "absent.json should have existed_at_declaration=false"
                );
            }
            other => panic!("expected ReplaceProvider, got {:?}", other),
        }

        if let ProviderWatchCommand::ReplaceProvider { ack, .. } = replace_cmd {
            let _ = ack.send(());
        }

        let _ = refresh_handle
            .await
            .expect("refresh task panicked")
            .expect("refresh failed");
    }
}
