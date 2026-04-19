use crate::plugin_engine::host_api;
use crate::plugin_engine::manifest::LoadedPlugin;
use crate::plugin_engine::script_patch;
use rquickjs::{Array, Context, Ctx, Error, Object, Promise, Runtime, Value};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ProgressFormat {
    Percent,
    Dollars,
    Count { suffix: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum MetricLine {
    Text {
        label: String,
        value: String,
        color: Option<String>,
        subtitle: Option<String>,
    },
    Progress {
        label: String,
        used: f64,
        limit: f64,
        format: ProgressFormat,
        #[serde(rename = "resetsAt")]
        resets_at: Option<String>,
        #[serde(rename = "periodDurationMs")]
        period_duration_ms: Option<u64>,
        color: Option<String>,
    },
    Badge {
        label: String,
        text: String,
        color: Option<String>,
        subtitle: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginOutput {
    pub provider_id: String,
    pub display_name: String,
    pub plan: Option<String>,
    pub lines: Vec<MetricLine>,
    pub icon_url: String,
}

pub fn run_probe(
    plugin: &LoadedPlugin,
    app_data_dir: &Path,
    app_version: &str,
    plugin_overrides_dir: Option<&Path>,
) -> PluginOutput {
    let fallback = error_output(plugin, "runtime error".to_string());

    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(_) => return fallback,
    };

    let ctx = match Context::full(&rt) {
        Ok(ctx) => ctx,
        Err(_) => return fallback,
    };

    let plugin_id = plugin.manifest.id.clone();
    let display_name = plugin.manifest.name.clone();
    let override_script = match load_plugin_override(plugin, plugin_overrides_dir) {
        Ok(value) => value,
        Err(err) => return error_output(plugin, format!("plugin override failed: {}", err)),
    };

    let entry_script = match script_patch::transform_plugin_script(
        &plugin_id,
        &plugin.entry_script,
        override_script
            .as_ref()
            .map(|loaded| loaded.script.as_str()),
    ) {
        Ok(result) => {
            if !result.patched_functions.is_empty() {
                log::info!(
                    "[plugin:{}] AST patch applied: {}",
                    plugin_id,
                    result.patched_functions.join(",")
                );
            }
            result.script
        }
        Err(err) => return error_output(plugin, format!("script transform failed: {}", err)),
    };
    let icon_url = plugin.icon_data_url.clone();
    let app_data = app_data_dir.to_path_buf();

    ctx.with(|ctx| {
        if host_api::inject_host_api(&ctx, &plugin_id, &app_data, app_version).is_err() {
            return error_output(plugin, "host api injection failed".to_string());
        }
        if host_api::patch_http_wrapper(&ctx).is_err() {
            return error_output(plugin, "http wrapper patch failed".to_string());
        }
        if host_api::patch_ls_wrapper(&ctx).is_err() {
            return error_output(plugin, "ls wrapper patch failed".to_string());
        }
        if host_api::patch_ccusage_wrapper(&ctx).is_err() {
            return error_output(plugin, "ccusage wrapper patch failed".to_string());
        }
        if host_api::inject_utils(&ctx).is_err() {
            return error_output(plugin, "utils injection failed".to_string());
        }

        if ctx.eval::<(), _>(entry_script.as_bytes()).is_err() {
            return error_output(plugin, "script eval failed".to_string());
        }

        if let Err(err) = apply_plugin_override(&ctx, &plugin_id, override_script.as_ref()) {
            return error_output(plugin, format!("plugin override failed: {}", err));
        }

        let globals = ctx.globals();
        let plugin_obj: Object = match globals.get("__openusage_plugin") {
            Ok(obj) => obj,
            Err(_) => return error_output(plugin, "missing __openusage_plugin".to_string()),
        };

        let probe_fn: rquickjs::Function = match plugin_obj.get("probe") {
            Ok(f) => f,
            Err(_) => return error_output(plugin, "missing probe()".to_string()),
        };

        let probe_ctx: Value = globals
            .get("__openusage_ctx")
            .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));

        let result_value: Value = match probe_fn.call((probe_ctx,)) {
            Ok(r) => r,
            Err(_) => return error_output(plugin, extract_error_string(&ctx)),
        };
        let result: Object = if result_value.is_promise() {
            let promise: Promise = match result_value.into_promise() {
                Some(promise) => promise,
                None => {
                    return error_output(plugin, "probe() returned invalid promise".to_string());
                }
            };
            match promise.finish::<Object>() {
                Ok(obj) => obj,
                Err(Error::WouldBlock) => {
                    return error_output(plugin, "probe() returned unresolved promise".to_string());
                }
                Err(_) => return error_output(plugin, extract_error_string(&ctx)),
            }
        } else {
            match result_value.into_object() {
                Some(obj) => obj,
                None => return error_output(plugin, "probe() returned non-object".to_string()),
            }
        };

        let plan: Option<String> = result
            .get::<_, String>("plan")
            .ok()
            .filter(|s| !s.is_empty());

        let lines = match parse_lines(&result) {
            Ok(lines) if !lines.is_empty() => lines,
            Ok(_) => vec![error_line("no lines returned".to_string())],
            Err(msg) => vec![error_line(msg)],
        };

        PluginOutput {
            provider_id: plugin_id,
            display_name,
            plan,
            lines,
            icon_url,
        }
    })
}

struct LoadedOverrideScript {
    path: PathBuf,
    script: String,
}

fn load_plugin_override(
    plugin: &LoadedPlugin,
    plugin_overrides_dir: Option<&Path>,
) -> Result<Option<LoadedOverrideScript>, String> {
    let Some(overrides_dir) = plugin_overrides_dir else {
        return Ok(None);
    };

    let Some(override_path) = resolve_plugin_override_path(overrides_dir, &plugin.manifest.id)
    else {
        return Ok(None);
    };

    let override_script = std::fs::read_to_string(&override_path)
        .map_err(|e| format!("failed to read {}: {}", override_path.display(), e))?;
    if override_script.trim().is_empty() {
        log::warn!(
            "[plugin:{}] override file is empty: {}",
            plugin.manifest.id,
            override_path.display()
        );
        return Ok(None);
    }

    Ok(Some(LoadedOverrideScript {
        path: override_path,
        script: override_script,
    }))
}

fn apply_plugin_override(
    ctx: &Ctx<'_>,
    plugin_id: &str,
    override_script: Option<&LoadedOverrideScript>,
) -> Result<(), String> {
    let Some(override_script) = override_script else {
        return Ok(());
    };

    inject_override_api(ctx, plugin_id)?;

    if ctx
        .eval::<(), _>(override_script.script.as_bytes())
        .is_err()
    {
        return Err(extract_error_string(ctx));
    }

    log::info!(
        "[plugin:{}] override loaded: {}",
        plugin_id,
        override_script.path.display()
    );
    Ok(())
}

fn resolve_plugin_override_path(overrides_dir: &Path, plugin_id: &str) -> Option<PathBuf> {
    if plugin_id.contains('/') || plugin_id.contains('\\') {
        log::warn!(
            "invalid plugin id for override path resolution: {}",
            plugin_id
        );
        return None;
    }

    let candidates = [
        overrides_dir.join(format!("{}.js", plugin_id)),
        overrides_dir.join(format!("{}.override.js", plugin_id)),
        overrides_dir.join(plugin_id).join("override.js"),
    ];

    candidates.into_iter().find(|path| path.is_file())
}

fn inject_override_api(ctx: &Ctx<'_>, plugin_id: &str) -> Result<(), String> {
    let plugin_id_json = serde_json::to_string(plugin_id)
        .map_err(|e| format!("failed to encode plugin id for override api: {}", e))?;

    let bootstrap_script = format!(
        r#"
        (function() {{
            var plugin = globalThis.__openusage_plugin;
            if (!plugin || typeof plugin !== "object") {{
                throw "missing __openusage_plugin before override init";
            }}
            if (typeof plugin.probe !== "function") {{
                throw "missing probe() before override init";
            }}

            var originalProbe = plugin.probe.bind(plugin);

            globalThis.__openusage_override = {{
                pluginId: {plugin_id_json},
                originalProbe: originalProbe,
                replaceProbe: function(replacement) {{
                    if (typeof replacement !== "function") {{
                        throw "replaceProbe expects a function";
                    }}
                    plugin.probe = function(ctx) {{
                        return replacement(ctx, originalProbe);
                    }};
                    return plugin.probe;
                }},
                wrapProbe: function(wrapper) {{
                    if (typeof wrapper !== "function") {{
                        throw "wrapProbe expects a function";
                    }}
                    var previousProbe = plugin.probe.bind(plugin);
                    plugin.probe = function(ctx) {{
                        return wrapper(ctx, previousProbe, originalProbe);
                    }};
                    return plugin.probe;
                }},
                resetProbe: function() {{
                    plugin.probe = originalProbe;
                    return plugin.probe;
                }}
            }};
        }})();
        "#
    );

    if ctx.eval::<(), _>(bootstrap_script.as_bytes()).is_err() {
        return Err(extract_error_string(ctx));
    }

    Ok(())
}

fn parse_lines(result: &Object) -> Result<Vec<MetricLine>, String> {
    let lines: Array = result
        .get("lines")
        .map_err(|_| "missing lines".to_string())?;

    let mut out = Vec::new();
    let len = lines.len();
    for idx in 0..len {
        let line: Object = lines
            .get(idx)
            .map_err(|_| format!("invalid line at index {}", idx))?;

        let line_type: String = line.get("type").unwrap_or_default();
        let label = line.get::<_, String>("label").unwrap_or_default();
        let color = line.get::<_, String>("color").ok();
        let subtitle = line.get::<_, String>("subtitle").ok();

        match line_type.as_str() {
            "text" => {
                let value = line.get::<_, String>("value").unwrap_or_default();
                out.push(MetricLine::Text {
                    label,
                    value,
                    color,
                    subtitle,
                });
            }
            "progress" => {
                let used_value: Value = match line.get("used") {
                    Ok(v) => v,
                    Err(_) => {
                        out.push(error_line(format!(
                            "progress line at index {} missing used",
                            idx
                        )));
                        continue;
                    }
                };
                let used = match used_value.as_number() {
                    Some(n) => n,
                    None => {
                        out.push(error_line(format!(
                            "progress line at index {} invalid used (expected number)",
                            idx
                        )));
                        continue;
                    }
                };

                let limit_value: Value = match line.get("limit") {
                    Ok(v) => v,
                    Err(_) => {
                        out.push(error_line(format!(
                            "progress line at index {} missing limit",
                            idx
                        )));
                        continue;
                    }
                };
                let limit = match limit_value.as_number() {
                    Some(n) => n,
                    None => {
                        out.push(error_line(format!(
                            "progress line at index {} invalid limit (expected number)",
                            idx
                        )));
                        continue;
                    }
                };

                if !used.is_finite() || used < 0.0 {
                    out.push(error_line(format!(
                        "progress line at index {} invalid used: {}",
                        idx, used
                    )));
                    continue;
                }
                if !limit.is_finite() || limit <= 0.0 {
                    out.push(error_line(format!(
                        "progress line at index {} invalid limit: {}",
                        idx, limit
                    )));
                    continue;
                }

                let format_obj: Object = match line.get("format") {
                    Ok(obj) => obj,
                    Err(_) => {
                        out.push(error_line(format!(
                            "progress line at index {} missing format",
                            idx
                        )));
                        continue;
                    }
                };
                let kind_value: Value = match format_obj.get("kind") {
                    Ok(v) => v,
                    Err(_) => {
                        out.push(error_line(format!(
                            "progress line at index {} missing format.kind",
                            idx
                        )));
                        continue;
                    }
                };
                let kind = match kind_value.as_string() {
                    Some(s) => s.to_string().unwrap_or_default(),
                    None => {
                        out.push(error_line(format!(
                            "progress line at index {} invalid format.kind (expected string)",
                            idx
                        )));
                        continue;
                    }
                };
                let format = match kind.as_str() {
                    "percent" => {
                        if limit != 100.0 {
                            out.push(error_line(format!(
                                "progress line at index {}: percent format requires limit=100 (got {})",
                                idx, limit
                            )));
                            continue;
                        }
                        ProgressFormat::Percent
                    }
                    "dollars" => ProgressFormat::Dollars,
                    "count" => {
                        let suffix_value: Value = match format_obj.get("suffix") {
                            Ok(v) => v,
                            Err(_) => {
                                out.push(error_line(format!(
                                    "progress line at index {}: count format missing suffix",
                                    idx
                                )));
                                continue;
                            }
                        };
                        let suffix = match suffix_value.as_string() {
                            Some(s) => s.to_string().unwrap_or_default(),
                            None => {
                                out.push(error_line(format!(
                                    "progress line at index {}: count format suffix must be a string",
                                    idx
                                )));
                                continue;
                            }
                        };
                        let suffix = suffix.trim().to_string();
                        if suffix.is_empty() {
                            out.push(error_line(format!(
                                "progress line at index {}: count format suffix must be non-empty",
                                idx
                            )));
                            continue;
                        }
                        ProgressFormat::Count { suffix }
                    }
                    _ => {
                        out.push(error_line(format!(
                            "progress line at index {} invalid format.kind: {}",
                            idx, kind
                        )));
                        continue;
                    }
                };

                let resets_at = match line.get::<_, Value>("resetsAt") {
                    Ok(v) => {
                        if v.is_null() || v.is_undefined() {
                            None
                        } else if let Some(s) = v.as_string() {
                            let raw = s.to_string().unwrap_or_default();
                            let value = raw.trim().to_string();
                            if value.is_empty() {
                                None
                            } else {
                                let parsed = time::OffsetDateTime::parse(
                                    &value,
                                    &time::format_description::well_known::Rfc3339,
                                );
                                if parsed.is_ok() {
                                    Some(value)
                                } else {
                                    // ISO-like but missing timezone: assume UTC.
                                    let is_missing_tz =
                                        value.split_once('T').is_some_and(|(_, tail)| {
                                            !value.ends_with('Z')
                                                && !tail.contains('+')
                                                && !tail.contains('-')
                                        });
                                    if is_missing_tz {
                                        let with_z = format!("{}Z", value);
                                        let parsed_with_z = time::OffsetDateTime::parse(
                                            &with_z,
                                            &time::format_description::well_known::Rfc3339,
                                        );
                                        if parsed_with_z.is_ok() {
                                            Some(with_z)
                                        } else {
                                            log::warn!(
                                                "invalid resetsAt at index {} (value='{}'), omitting",
                                                idx,
                                                raw
                                            );
                                            None
                                        }
                                    } else {
                                        log::warn!(
                                            "invalid resetsAt at index {} (value='{}'), omitting",
                                            idx,
                                            raw
                                        );
                                        None
                                    }
                                }
                            }
                        } else {
                            log::warn!("invalid resetsAt at index {} (non-string), omitting", idx);
                            None
                        }
                    }
                    Err(_) => None,
                };

                // Parse optional periodDurationMs
                let period_duration_ms: Option<u64> = match line.get::<_, Value>("periodDurationMs")
                {
                    Ok(val) => {
                        if val.is_null() || val.is_undefined() {
                            None
                        } else if let Some(n) = val.as_number() {
                            let ms = n as u64;
                            if ms > 0 {
                                Some(ms)
                            } else {
                                log::warn!(
                                    "periodDurationMs at index {} must be positive, omitting",
                                    idx
                                );
                                None
                            }
                        } else {
                            log::warn!(
                                "invalid periodDurationMs at index {} (non-number), omitting",
                                idx
                            );
                            None
                        }
                    }
                    Err(_) => None,
                };

                out.push(MetricLine::Progress {
                    label,
                    used,
                    limit,
                    format,
                    resets_at,
                    period_duration_ms,
                    color,
                });
            }
            "badge" => {
                let text = line.get::<_, String>("text").unwrap_or_default();
                out.push(MetricLine::Badge {
                    label,
                    text,
                    color,
                    subtitle,
                });
            }
            _ => {
                out.push(error_line(format!(
                    "unknown line type at index {}: {}",
                    idx, line_type
                )));
            }
        }
    }

    Ok(out)
}

fn error_output(plugin: &LoadedPlugin, message: String) -> PluginOutput {
    PluginOutput {
        provider_id: plugin.manifest.id.clone(),
        display_name: plugin.manifest.name.clone(),
        plan: None,
        lines: vec![error_line(message)],
        icon_url: plugin.icon_data_url.clone(),
    }
}

fn extract_error_string(ctx: &Ctx<'_>) -> String {
    let exc = ctx.catch();
    if exc.is_null() || exc.is_undefined() {
        return "The plugin failed, try again or contact plugin author.".to_string();
    }
    if let Some(str_val) = exc.as_string() {
        let message: String = str_val.to_string().unwrap_or_default();
        let trimmed = message.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "The plugin failed, try again or contact plugin author.".to_string()
}

fn error_line(message: String) -> MetricLine {
    MetricLine::Badge {
        label: "Error".to_string(),
        text: message,
        color: Some("#ef4444".to_string()),
        subtitle: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_engine::manifest::{LoadedPlugin, PluginManifest};
    use serde_json::Value as JsonValue;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    fn temp_app_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("openusage-test-{}-{}", label, nanos))
    }

    fn error_text(output: PluginOutput) -> String {
        match output.lines.first() {
            Some(MetricLine::Badge { text, .. }) => text.clone(),
            other => panic!("expected error badge, got {:?}", other),
        }
    }

    #[test]
    fn run_probe_returns_thrown_string_from_sync_error() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe() {
                    throw "boom";
                }
            };
            "#,
        );
        let output = run_probe(&plugin, &temp_app_dir("sync"), "0.0.0", None);
        assert_eq!(error_text(output), "boom");
    }

    #[test]
    fn run_probe_returns_thrown_string_from_async_error() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe: async function () {
                    throw "boom";
                }
            };
            "#,
        );
        let output = run_probe(&plugin, &temp_app_dir("async"), "0.0.0", None);
        assert_eq!(error_text(output), "boom");
    }

    #[test]
    fn run_probe_applies_plugin_override_wrapper() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        lines: [ctx.line.badge({ label: "Status", text: "original" })]
                    };
                }
            };
            "#,
        );

        let app_data_dir = temp_app_dir("override-app");
        let overrides_dir = temp_app_dir("override-dir");
        std::fs::create_dir_all(&overrides_dir).expect("create overrides dir");
        std::fs::write(
            overrides_dir.join("test.js"),
            r#"
            if (!globalThis.__openusage_override) {
              throw "missing __openusage_override";
            }

            globalThis.__openusage_override.wrapProbe(function(ctx, currentProbe, originalProbe) {
              const result = originalProbe(ctx);
              result.lines.push(ctx.line.badge({ label: "Override", text: "applied" }));
              return result;
            });
            "#,
        )
        .expect("write override script");

        let output = run_probe(
            &plugin,
            &app_data_dir,
            "0.0.0",
            Some(overrides_dir.as_path()),
        );

        let has_override_badge = output.lines.iter().any(|line| {
            matches!(
                line,
                MetricLine::Badge { label, text, .. }
                    if label == "Override" && text == "applied"
            )
        });
        assert!(
            has_override_badge,
            "expected override badge from plugin override wrapper"
        );
    }

    #[test]
    fn progress_resets_at_serializes_as_resets_at_camelcase() {
        let line = MetricLine::Progress {
            label: "Session".to_string(),
            used: 1.0,
            limit: 100.0,
            format: ProgressFormat::Percent,
            resets_at: Some("2099-01-01T00:00:00.000Z".to_string()),
            period_duration_ms: None,
            color: None,
        };

        let json: JsonValue = serde_json::to_value(&line).expect("serialize");
        let obj = json.as_object().expect("object");
        assert!(obj.get("resetsAt").is_some(), "expected resetsAt key");
        assert!(
            obj.get("resets_at").is_none(),
            "did not expect resets_at key"
        );
    }
}
