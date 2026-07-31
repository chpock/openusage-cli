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
    pub account: crate::plugin_engine::runtime::AccountRef,
}

impl CachedPluginSnapshot {
    fn from_output(output: runtime::PluginOutput) -> Self {
        Self {
            provider_id: output.provider_id,
            display_name: output.display_name,
            plan: output.plan,
            lines: output.lines,
            fetched_at: now_iso(),
            account: output.account,
        }
    }
}

pub struct DaemonState {
    plugins: Vec<LoadedPlugin>,
    app_data_dir: PathBuf,
    app_version: String,
    plugin_overrides_dir: Option<PathBuf>,
    cache: RwLock<HashMap<String, Vec<CachedPluginSnapshot>>>,
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
        let mut results = Vec::new();
        for id in &selected_ids {
            if let Some(snapshots) = cache.get(id) {
                results.extend(snapshots.iter().cloned());
            }
        }
        results
    }

    pub async fn cached_one(&self, plugin_id: &str) -> Option<CachedPluginSnapshot> {
        let cache = self.cache.read().await;
        let snapshots = cache.get(plugin_id)?;

        // Prefer the default account when present.
        if let Some(snapshot) = snapshots.iter().find(|s| s.account.id == "default") {
            return Some(snapshot.clone());
        }

        // No default account: return the sole snapshot if exactly one exists.
        // Multiple non-default accounts means selection is deferred.
        if snapshots.len() == 1 {
            return Some(snapshots[0].clone());
        }

        None
    }

    /// Atomically replace one provider's entire cache vector.
    /// Accepts an empty vector to clear stale accounts from a valid empty
    /// discovery result. This is the single-provider production operation
    /// used by `refresh`.
    async fn replace_provider_cache(
        &self,
        provider_id: &str,
        snapshots: Vec<CachedPluginSnapshot>,
    ) {
        let mut cache = self.cache.write().await;
        cache.insert(provider_id.to_string(), snapshots);
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
        let mut refreshed_providers: Vec<String> = Vec::with_capacity(selected.len());

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

            let n_outputs = probe_result.outputs.len();
            log::debug!(
                "probe finished for plugin {}: outputs={}, subscriptions={}",
                probe_result
                    .outputs
                    .first()
                    .map(|o| o.provider_id.as_str())
                    .unwrap_or(&plugin_id),
                n_outputs,
                probe_result.subscriptions.len(),
            );

            // Provider ID from the first output (all share the same provider).
            let provider_id = probe_result
                .outputs
                .first()
                .map(|o| o.provider_id.clone())
                .unwrap_or_else(|| plugin_id.clone());

            refreshed_providers.push(provider_id.clone());

            // --- Provider file-monitor: replace subscriptions after probe ---
            if let Some(cmd_tx) = &self.monitor_cmd_tx {
                let (ack, rx) = oneshot::channel();
                if cmd_tx
                    .send(ProviderWatchCommand::ReplaceProvider {
                        provider_id: provider_id.clone(),
                        files: probe_result.subscriptions.clone(),
                        ack,
                    })
                    .is_err()
                {
                    log::warn!(
                        "[daemon] monitor channel closed before replace for {}",
                        provider_id
                    );
                } else {
                    if rx.await.is_err() {
                        log::warn!(
                            "[daemon] {} acknowledgement canceled for {}",
                            "replace",
                            provider_id
                        );
                    }
                }
            }

            for output in probe_result.outputs {
                snapshots.push(CachedPluginSnapshot::from_output(output));
            }
        }

        // Atomically replace each selected provider's cache vector,
        // even for providers with zero outputs (valid empty discovery).
        // Uses the production single-provider operation.
        {
            for provider_id in &refreshed_providers {
                let provider_snapshots: Vec<CachedPluginSnapshot> = snapshots
                    .iter()
                    .filter(|s| s.provider_id == *provider_id)
                    .cloned()
                    .collect();
                self.replace_provider_cache(provider_id, provider_snapshots)
                    .await;
            }
        }

        log::debug!(
            "refresh finished: updated_snapshots={}, cache_size={}",
            snapshots.len(),
            self.cache.read().await.len()
        );

        Ok(snapshots)
    }

    pub async fn has_cached_for(&self, plugin_ids: Option<&[String]>) -> bool {
        let cache = self.cache.read().await;
        let selected_ids = self.resolve_plugin_ids(plugin_ids);
        selected_ids
            .iter()
            .all(|id| cache.contains_key(id) && !cache[id].is_empty())
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

        for snapshots in cache.values() {
            for snapshot in snapshots {
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
                        let effective_reset =
                            reset_time + time::Duration::seconds(margin_secs as i64);
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

        for (provider_id, snapshots) in cache.iter() {
            let has_past_reset = snapshots.iter().any(|snapshot| {
                snapshot.lines.iter().any(|line| {
                    if let MetricLine::Progress {
                        resets_at: Some(resets_at_str),
                        ..
                    } = line
                        && let Ok(reset_time) = time::OffsetDateTime::parse(
                            resets_at_str,
                            &time::format_description::well_known::Rfc3339,
                        )
                    {
                        let effective_reset =
                            reset_time + time::Duration::seconds(margin_secs as i64);
                        effective_reset <= now
                    } else {
                        false
                    }
                })
            });

            if has_past_reset {
                provider_ids.push(provider_id.clone());
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

    #[cfg(test)]
    /// Replace the plugins list (test-only, no concurrent access).
    pub fn set_plugins(&mut self, plugins: Vec<LoadedPlugin>) {
        self.plugins = plugins;
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
    use crate::plugin_engine::runtime::AccountRef;
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
            account: crate::plugin_engine::runtime::AccountRef::default_account(),
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
            cache
                .entry(item.provider_id.clone())
                .or_default()
                .push(item);
        }
        drop(cache);
        state
    }

    async fn state_with_plugins_and_cache(
        plugin_ids: Vec<&str>,
        snapshots: Vec<CachedPluginSnapshot>,
    ) -> DaemonState {
        let plugins: Vec<LoadedPlugin> = plugin_ids
            .into_iter()
            .map(|id| LoadedPlugin {
                manifest: PluginManifest {
                    schema_version: 1,
                    id: id.to_string(),
                    name: id.to_string(),
                    version: "0.0.0".to_string(),
                    entry: "plugin.js".to_string(),
                    icon: "icon.svg".to_string(),
                    brand_color: None,
                    lines: vec![],
                    links: vec![],
                },
                plugin_dir: PathBuf::from("."),
                entry_script: String::new(),
                icon_data_url: "data:image/svg+xml;base64,".to_string(),
            })
            .collect();
        let state = DaemonState::new(
            plugins,
            PathBuf::from("."),
            "0.0.0-test".to_string(),
            None,
            None,
        );
        let mut cache = state.cache.write().await;
        for item in snapshots {
            cache
                .entry(item.provider_id.clone())
                .or_default()
                .push(item);
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

    #[tokio::test]
    async fn refresh_preserves_custom_account_from_plugin() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { id: "custom-id", displayName: "Custom Name" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );

        let app_data = std::env::temp_dir().join("daemon-custom-account-test");
        std::fs::create_dir_all(&app_data).ok();

        let state = Arc::new(DaemonState::new(
            vec![plugin],
            app_data,
            "0.0.0-test".to_string(),
            None,
            None,
        ));

        let snapshots = state.refresh(None).await.expect("refresh should succeed");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].provider_id, "test");
        assert_eq!(
            snapshots[0].account,
            AccountRef {
                id: "custom-id".to_string(),
                display_name: "Custom Name".to_string(),
            },
            "refresh must preserve custom account from plugin"
        );

        // Verify the cached snapshot also has the custom account.
        let cached = state.cached(Some(&["test".to_string()])).await;
        assert_eq!(cached.len(), 1, "snapshot should be cached");
        assert_eq!(
            cached[0].account,
            AccountRef {
                id: "custom-id".to_string(),
                display_name: "Custom Name".to_string(),
            },
            "cached snapshot must preserve custom account"
        );
    }

    #[tokio::test]
    async fn cache_replacement_removes_stale_account() {
        let state = DaemonState::new(
            Vec::new(),
            PathBuf::from("."),
            "0.0.0-test".to_string(),
            None,
            None,
        );

        // Insert two snapshots for the same provider (simulating two accounts).
        {
            let mut cache = state.cache.write().await;
            cache.insert(
                "test-provider".to_string(),
                vec![
                    CachedPluginSnapshot {
                        provider_id: "test-provider".to_string(),
                        display_name: "Test".to_string(),
                        plan: None,
                        lines: vec![text_line()],
                        fetched_at: now_iso(),
                        account: AccountRef::default_account(),
                    },
                    CachedPluginSnapshot {
                        provider_id: "test-provider".to_string(),
                        display_name: "Test".to_string(),
                        plan: None,
                        lines: vec![text_line()],
                        fetched_at: now_iso(),
                        account: AccountRef {
                            id: "secondary".to_string(),
                            display_name: "Secondary".to_string(),
                        },
                    },
                ],
            );
        }

        // Verify two snapshots exist.
        {
            let cache = state.cache.read().await;
            assert_eq!(
                cache.get("test-provider").map(|v| v.len()),
                Some(2),
                "should have two account snapshots before replacement"
            );
        }

        // Replace with a single snapshot using the single-provider operation.
        state
            .replace_provider_cache(
                "test-provider",
                vec![CachedPluginSnapshot {
                    provider_id: "test-provider".to_string(),
                    display_name: "Test".to_string(),
                    plan: None,
                    lines: vec![text_line()],
                    fetched_at: now_iso(),
                    account: AccountRef::default_account(),
                }],
            )
            .await;

        // Verify only one snapshot remains (stale account removed).
        {
            let cache = state.cache.read().await;
            let snapshots = cache.get("test-provider").expect("should have entry");
            assert_eq!(
                snapshots.len(),
                1,
                "stale account should be removed after replacement"
            );
            assert_eq!(snapshots[0].account.id, "default");
        }
    }

    #[tokio::test]
    async fn cached_flattened_collection_respects_plugin_order_then_stored_account_order() {
        // Provider order contract: cached() returns items in the order of
        // selected/resolved plugin IDs (loaded order when no filter is given),
        // then in the order stored in each provider's account vector.
        let state = state_with_plugins_and_cache(
            vec!["alpha", "beta"],
            vec![
                CachedPluginSnapshot {
                    provider_id: "alpha".to_string(),
                    display_name: "Alpha".to_string(),
                    plan: None,
                    lines: vec![text_line()],
                    fetched_at: now_iso(),
                    account: AccountRef::default_account(),
                },
                CachedPluginSnapshot {
                    provider_id: "alpha".to_string(),
                    display_name: "Alpha".to_string(),
                    plan: None,
                    lines: vec![text_line()],
                    fetched_at: now_iso(),
                    account: AccountRef {
                        id: "secondary".to_string(),
                        display_name: "Secondary".to_string(),
                    },
                },
                CachedPluginSnapshot {
                    provider_id: "beta".to_string(),
                    display_name: "Beta".to_string(),
                    plan: None,
                    lines: vec![text_line()],
                    fetched_at: now_iso(),
                    account: AccountRef::default_account(),
                },
            ],
        )
        .await;

        // Use the production cached() method with no filter.
        let cached = state.cached(None).await;

        assert_eq!(cached.len(), 3, "should have 3 total snapshots");
        // Provider order: alpha (loaded first), then beta (loaded second).
        assert_eq!(cached[0].provider_id, "alpha");
        assert_eq!(cached[1].provider_id, "alpha");
        assert_eq!(cached[2].provider_id, "beta");
        // Account order within alpha: stored order = default, then secondary.
        assert_eq!(cached[0].account.id, "default");
        assert_eq!(cached[1].account.id, "secondary");
    }

    #[tokio::test]
    async fn cached_with_provider_filter_returns_only_that_providers_accounts() {
        // Provider-scoped filtering: cached(Some(["alpha"])) must return only
        // alpha's account snapshots in stored order, without beta.
        let state = state_with_plugins_and_cache(
            vec!["alpha", "beta"],
            vec![
                CachedPluginSnapshot {
                    provider_id: "alpha".to_string(),
                    display_name: "Alpha".to_string(),
                    plan: None,
                    lines: vec![text_line()],
                    fetched_at: now_iso(),
                    account: AccountRef::default_account(),
                },
                CachedPluginSnapshot {
                    provider_id: "alpha".to_string(),
                    display_name: "Alpha".to_string(),
                    plan: None,
                    lines: vec![text_line()],
                    fetched_at: now_iso(),
                    account: AccountRef {
                        id: "secondary".to_string(),
                        display_name: "Secondary".to_string(),
                    },
                },
                CachedPluginSnapshot {
                    provider_id: "beta".to_string(),
                    display_name: "Beta".to_string(),
                    plan: None,
                    lines: vec![text_line()],
                    fetched_at: now_iso(),
                    account: AccountRef::default_account(),
                },
            ],
        )
        .await;

        let alpha_only = state.cached(Some(&["alpha".to_string()])).await;

        assert_eq!(
            alpha_only.len(),
            2,
            "should return both alpha accounts, not beta"
        );
        assert_eq!(alpha_only[0].provider_id, "alpha");
        assert_eq!(alpha_only[1].provider_id, "alpha");
        // Stored account order: default, then secondary.
        assert_eq!(alpha_only[0].account.id, "default");
        assert_eq!(alpha_only[1].account.id, "secondary");
    }

    #[tokio::test]
    async fn cached_one_selects_default_irrespective_of_vector_order() {
        let state = DaemonState::new(
            Vec::new(),
            PathBuf::from("."),
            "0.0.0-test".to_string(),
            None,
            None,
        );

        // Insert with non-default account first, default second.
        {
            let mut cache = state.cache.write().await;
            cache.insert(
                "test-provider".to_string(),
                vec![
                    CachedPluginSnapshot {
                        provider_id: "test-provider".to_string(),
                        display_name: "Test".to_string(),
                        plan: None,
                        lines: vec![text_line()],
                        fetched_at: now_iso(),
                        account: AccountRef {
                            id: "secondary".to_string(),
                            display_name: "Secondary".to_string(),
                        },
                    },
                    CachedPluginSnapshot {
                        provider_id: "test-provider".to_string(),
                        display_name: "Test".to_string(),
                        plan: None,
                        lines: vec![text_line()],
                        fetched_at: now_iso(),
                        account: AccountRef::default_account(),
                    },
                ],
            );
        }

        let result = state.cached_one("test-provider").await;
        assert!(result.is_some(), "cached_one should find a snapshot");
        assert_eq!(
            result.unwrap().account.id,
            "default",
            "should select default account regardless of vector position"
        );
    }

    #[tokio::test]
    async fn cached_one_returns_sole_non_default_account() {
        let state = DaemonState::new(
            Vec::new(),
            PathBuf::from("."),
            "0.0.0-test".to_string(),
            None,
            None,
        );

        // Single non-default account snapshot.
        {
            let mut cache = state.cache.write().await;
            cache.insert(
                "custom-provider".to_string(),
                vec![CachedPluginSnapshot {
                    provider_id: "custom-provider".to_string(),
                    display_name: "Custom".to_string(),
                    plan: None,
                    lines: vec![text_line()],
                    fetched_at: now_iso(),
                    account: AccountRef {
                        id: "custom-account".to_string(),
                        display_name: "Custom Account".to_string(),
                    },
                }],
            );
        }

        let result = state.cached_one("custom-provider").await;
        assert!(
            result.is_some(),
            "cached_one should return the sole non-default snapshot"
        );
        assert_eq!(result.unwrap().account.id, "custom-account");
    }

    #[tokio::test]
    async fn cached_one_returns_none_for_multiple_non_default_accounts() {
        let state = DaemonState::new(
            Vec::new(),
            PathBuf::from("."),
            "0.0.0-test".to_string(),
            None,
            None,
        );

        // Two non-default account snapshots.
        {
            let mut cache = state.cache.write().await;
            cache.insert(
                "multi-provider".to_string(),
                vec![
                    CachedPluginSnapshot {
                        provider_id: "multi-provider".to_string(),
                        display_name: "Multi".to_string(),
                        plan: None,
                        lines: vec![text_line()],
                        fetched_at: now_iso(),
                        account: AccountRef {
                            id: "account-a".to_string(),
                            display_name: "Account A".to_string(),
                        },
                    },
                    CachedPluginSnapshot {
                        provider_id: "multi-provider".to_string(),
                        display_name: "Multi".to_string(),
                        plan: None,
                        lines: vec![text_line()],
                        fetched_at: now_iso(),
                        account: AccountRef {
                            id: "account-b".to_string(),
                            display_name: "Account B".to_string(),
                        },
                    },
                ],
            );
        }

        let result = state.cached_one("multi-provider").await;
        assert!(
            result.is_none(),
            "cached_one should return None for multiple non-default accounts"
        );
    }

    #[tokio::test]
    async fn cached_one_default_preferred_over_sole_non_default() {
        // When both default and non-default exist, default is preferred.
        let state = state_with_cache(vec![
            CachedPluginSnapshot {
                provider_id: "mixed".to_string(),
                display_name: "Mixed".to_string(),
                plan: None,
                lines: vec![text_line()],
                fetched_at: now_iso(),
                account: AccountRef {
                    id: "other".to_string(),
                    display_name: "Other".to_string(),
                },
            },
            CachedPluginSnapshot {
                provider_id: "mixed".to_string(),
                display_name: "Mixed".to_string(),
                plan: None,
                lines: vec![text_line()],
                fetched_at: now_iso(),
                account: AccountRef::default_account(),
            },
        ])
        .await;

        let result = state.cached_one("mixed").await;
        assert!(result.is_some(), "cached_one should find default");
        assert_eq!(result.unwrap().account.id, "default");
    }

    #[tokio::test]
    async fn reset_sees_all_accounts_and_deduplicates_provider() {
        let state = state_with_cache(vec![
            CachedPluginSnapshot {
                provider_id: "stale-provider".to_string(),
                display_name: "Stale".to_string(),
                plan: None,
                lines: vec![progress_line(Some(iso_from_now(-20)))],
                fetched_at: now_iso(),
                account: AccountRef::default_account(),
            },
            CachedPluginSnapshot {
                provider_id: "stale-provider".to_string(),
                display_name: "Stale".to_string(),
                plan: None,
                lines: vec![progress_line(Some(iso_from_now(-10)))],
                fetched_at: now_iso(),
                account: AccountRef {
                    id: "secondary".to_string(),
                    display_name: "Secondary".to_string(),
                },
            },
            CachedPluginSnapshot {
                provider_id: "fresh-provider".to_string(),
                display_name: "Fresh".to_string(),
                plan: None,
                lines: vec![progress_line(Some(iso_from_now(120)))],
                fetched_at: now_iso(),
                account: AccountRef::default_account(),
            },
        ])
        .await;

        let stale_ids = state.provider_ids_with_past_resets(5).await;
        assert_eq!(
            stale_ids,
            vec!["stale-provider".to_string()],
            "should return each stale provider once, not per-account"
        );
    }

    #[tokio::test]
    async fn refresh_with_discovery_stores_two_accounts_then_empty_discovery_clears() {
        // Same-state regression: first refresh discovers two accounts;
        // after changing only the plugin script, a second refresh with
        // valid empty discovery clears that exact provider cache.
        // No fresh DaemonState or direct map mutation is substituted.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Account", value: ctx.account.id })] };
                },
                discoverAccounts(ctx) {
                    return [
                        { id: "work", displayName: "Work" },
                        { id: "personal", displayName: "Personal" }
                    ];
                }
            };
            "#,
        );

        let app_data = std::env::temp_dir().join("daemon-discovery-same-state-test");
        std::fs::create_dir_all(&app_data).ok();

        let mut state = Arc::new(DaemonState::new(
            vec![plugin],
            app_data,
            "0.0.0-test".to_string(),
            None,
            None,
        ));

        // First refresh: two accounts discovered.
        let snapshots = state
            .refresh(None)
            .await
            .expect("first refresh should succeed");
        assert_eq!(snapshots.len(), 2, "should have two account snapshots");
        assert_eq!(snapshots[0].account.id, "work");
        assert_eq!(snapshots[1].account.id, "personal");

        // Verify cache has two entries.
        let cached = state.cached(None).await;
        assert_eq!(cached.len(), 2);

        // Mutate the plugin on the same state to return empty discovery.
        // Only the test fixture input changes — same DaemonState, same cache.
        let empty_plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return [];
                }
            };
            "#,
        );
        // Replace the plugin in-place on the same state via test-only setter.
        // SAFETY: test-only; no concurrent access during this sequential test.
        let state_mut = Arc::get_mut(&mut state).expect("unique Arc ref in test");
        state_mut.set_plugins(vec![empty_plugin]);

        // Second refresh on the same state: empty discovery clears the provider.
        let snapshots2 = state
            .refresh(None)
            .await
            .expect("empty refresh should succeed");
        assert!(
            snapshots2.is_empty(),
            "empty discovery should produce zero snapshots"
        );

        // Cache should be empty for this provider on the same state.
        let cached2 = state.cached(None).await;
        assert!(
            cached2.is_empty(),
            "cache should be empty after empty discovery on same state"
        );
    }

    #[tokio::test]
    async fn refresh_monitor_lifecycle_unions_discovery_and_account_subscriptions() {
        // Monitor lifecycle: one ClearProvider/ReplaceProvider pair per provider,
        // with subscriptions from both discovery and account probes unioned.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    ctx.host.fs.subscribeFile("/tmp/probe-" + ctx.account.id + ".json");
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    ctx.host.fs.subscribeFile("/tmp/discovery-dep.json");
                    return [
                        { id: "a", displayName: "A" },
                        { id: "b", displayName: "B" }
                    ];
                }
            };
            "#,
        );

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let app_data = std::env::temp_dir().join("daemon-monitor-discovery-test");
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

        // 1) ClearProvider
        let clear_cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
            .await
            .expect("timeout waiting for ClearProvider")
            .expect("channel closed");
        match &clear_cmd {
            ProviderWatchCommand::ClearProvider {
                provider_id,
                ack: _,
            } => {
                assert_eq!(provider_id, "test");
            }
            other => panic!("expected ClearProvider, got {:?}", other),
        }
        if let ProviderWatchCommand::ClearProvider { ack, .. } = clear_cmd {
            let _ = ack.send(());
        }

        // 2) ReplaceProvider with unioned subscriptions
        // (discovery-dep.json + probe-a.json + probe-b.json)
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
                assert_eq!(files.len(), 3, "expected 3 unioned subscriptions");
                let paths: Vec<String> = files
                    .iter()
                    .map(|f| f.path.to_string_lossy().to_string())
                    .collect();
                assert!(paths.iter().any(|p| p.ends_with("discovery-dep.json")));
                assert!(paths.iter().any(|p| p.ends_with("probe-a.json")));
                assert!(paths.iter().any(|p| p.ends_with("probe-b.json")));
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
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].account.id, "a");
        assert_eq!(snapshots[1].account.id, "b");
    }

    #[tokio::test]
    async fn refresh_with_discovery_exception_replaces_two_accounts_with_provider_error() {
        // Same DaemonState: two discovered snapshots then a discovery
        // exception replaces them with exactly one default-account
        // provider error snapshot.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Account", value: ctx.account.id })] };
                },
                discoverAccounts(ctx) {
                    return [
                        { id: "work", displayName: "Work" },
                        { id: "personal", displayName: "Personal" }
                    ];
                }
            };
            "#,
        );

        let app_data = std::env::temp_dir().join("daemon-discovery-exception-test");
        std::fs::create_dir_all(&app_data).ok();

        let mut state = Arc::new(DaemonState::new(
            vec![plugin],
            app_data,
            "0.0.0-test".to_string(),
            None,
            None,
        ));

        // First refresh: two accounts discovered.
        let snapshots = state.refresh(None).await.expect("first refresh");
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].account.id, "work");
        assert_eq!(snapshots[1].account.id, "personal");
        let cached = state.cached(None).await;
        assert_eq!(cached.len(), 2);

        // Replace with a plugin that throws in discoverAccounts.
        let broken_plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    throw "discovery-exception";
                }
            };
            "#,
        );
        let state_mut = Arc::get_mut(&mut state).expect("unique Arc ref");
        state_mut.set_plugins(vec![broken_plugin]);

        // Second refresh: discovery exception produces one provider error.
        let snapshots2 = state
            .refresh(None)
            .await
            .expect("exception refresh should succeed");
        assert_eq!(
            snapshots2.len(),
            1,
            "exception should produce one error output"
        );
        assert_eq!(
            snapshots2[0].account,
            AccountRef::default_account(),
            "discovery exception must carry default account"
        );
        assert!(
            snapshots2[0].lines[0]
                .to_string()
                .contains("discovery-exception"),
            "error output must mention the exception"
        );

        // Cache must be replaced with exactly one error snapshot.
        let cached2 = state.cached(None).await;
        assert_eq!(cached2.len(), 1, "cache should have one error snapshot");
        assert_eq!(cached2[0].account, AccountRef::default_account());
    }
}
