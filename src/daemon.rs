use crate::plugin_engine::manifest::{LoadedPlugin, ManifestLine, PluginLink};
use crate::plugin_engine::runtime::{self, MetricLine};
use anyhow::anyhow;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokio::sync::{Mutex, RwLock};

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
}

impl DaemonState {
    pub fn new(
        plugins: Vec<LoadedPlugin>,
        app_data_dir: PathBuf,
        app_version: String,
        plugin_overrides_dir: Option<PathBuf>,
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
            log::debug!("running probe for plugin {}", plugin_id);
            let data_dir = self.app_data_dir.clone();
            let app_version = self.app_version.clone();
            let plugin_overrides_dir = self.plugin_overrides_dir.clone();
            let output = tokio::task::spawn_blocking(move || {
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
                "probe finished for plugin {}: lines={}",
                output.provider_id,
                output.lines.len()
            );
            snapshots.push(CachedPluginSnapshot::from_output(output));
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
