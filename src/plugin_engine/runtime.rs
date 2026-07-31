use crate::plugin_engine::host_api;
use crate::plugin_engine::manifest::LoadedPlugin;
use crate::plugin_engine::script_patch;
use rquickjs::object::Property;
use rquickjs::{Array, Context, Ctx, Error, Object, Runtime, Value};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

impl fmt::Display for MetricLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetricLine::Text { label, value, .. } => {
                write!(f, "{}: {}", label, value)
            }
            MetricLine::Progress {
                label, used, limit, ..
            } => {
                write!(f, "{}: {}/{}", label, used, limit)
            }
            MetricLine::Badge { label, text, .. } => {
                write!(f, "{}: {}", label, text)
            }
        }
    }
}

/// A serializable/equatable account reference carried by every plugin output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct AccountRef {
    pub id: String,
    pub display_name: String,
}

impl AccountRef {
    /// Default account used when no explicit account is provided.
    pub fn default_account() -> Self {
        Self {
            id: "default".to_string(),
            display_name: "default".to_string(),
        }
    }
}

impl Default for AccountRef {
    fn default() -> Self {
        Self::default_account()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginOutput {
    pub provider_id: String,
    pub display_name: String,
    pub plan: Option<String>,
    pub lines: Vec<MetricLine>,
    pub icon_url: String,
    #[serde(default)]
    pub account: AccountRef,
}

pub use crate::restart_watcher::FileSubscription;

/// Result of a probe execution, including file subscriptions collected
/// during the probe via `ctx.host.fs.subscribeFile()`.  Subscriptions may
/// be declared during plugin entry script evaluation, override evaluation,
/// or the probe/discovery call itself.
///
/// In legacy mode (no `discoverAccounts`), `outputs` contains one item.
/// In discovery mode, `outputs` contains one item per discovered account
/// (or zero for an empty valid discovery list, or one error output on
/// discovery failure).
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub outputs: Vec<PluginOutput>,
    pub subscriptions: Vec<FileSubscription>,
}

pub fn run_probe(
    plugin: &LoadedPlugin,
    app_data_dir: &Path,
    app_version: &str,
    plugin_overrides_dir: Option<&Path>,
) -> ProbeResult {
    let subs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fallback_subs = Arc::clone(&subs);
    let fallback = ProbeResult {
        outputs: vec![error_output(plugin, "runtime error".to_string())],
        subscriptions: std::mem::take(&mut *fallback_subs.lock().unwrap()),
    };

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
        Err(err) => {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    format!("plugin override failed: {}", err),
                )],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
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
        Err(err) => {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    format!("script transform failed: {}", err),
                )],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
    };
    let icon_url = plugin.icon_data_url.clone();
    let app_data = app_data_dir.to_path_buf();

    ctx.with(|ctx| {
        let subs_pass = Arc::clone(&subs);
        if host_api::inject_host_api(&ctx, &plugin_id, &app_data, app_version, subs_pass).is_err() {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    "host api injection failed".to_string(),
                )],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
        if host_api::patch_http_wrapper(&ctx).is_err() {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    "http wrapper patch failed".to_string(),
                )],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
        if host_api::patch_ls_wrapper(&ctx).is_err() {
            return ProbeResult {
                outputs: vec![error_output(plugin, "ls wrapper patch failed".to_string())],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
        if host_api::patch_ccusage_wrapper(&ctx).is_err() {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    "ccusage wrapper patch failed".to_string(),
                )],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
        if host_api::inject_utils(&ctx).is_err() {
            return ProbeResult {
                outputs: vec![error_output(plugin, "utils injection failed".to_string())],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }

        if ctx.eval::<(), _>(entry_script.as_bytes()).is_err() {
            return ProbeResult {
                outputs: vec![error_output(plugin, "script eval failed".to_string())],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }

        if let Err(err) = apply_plugin_override(&ctx, &plugin_id, override_script.as_ref()) {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    format!("plugin override failed: {}", err),
                )],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }

        let globals = ctx.globals();
        let plugin_obj: Object = match globals.get("__openusage_plugin") {
            Ok(obj) => obj,
            Err(_) => {
                return ProbeResult {
                    outputs: vec![error_output(
                        plugin,
                        "missing __openusage_plugin".to_string(),
                    )],
                    subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
                };
            }
        };

        let probe_fn: rquickjs::Function = match plugin_obj.get("probe") {
            Ok(f) => f,
            Err(_) => {
                return ProbeResult {
                    outputs: vec![error_output(plugin, "missing probe()".to_string())],
                    subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
                };
            }
        };

        let base_ctx: Value = globals
            .get("__openusage_ctx")
            .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));

        // Check for discoverAccounts capability (after override is applied).
        // Only absent/null/undefined selects legacy mode.
        // An accessor-thrown exception produces a provider error output.
        let discover_val: Value = match plugin_obj.get("discoverAccounts") {
            Ok(v) => v,
            Err(_) => {
                return ProbeResult {
                    outputs: vec![error_output(
                        plugin,
                        "discoverAccounts accessor threw an exception".to_string(),
                    )],
                    subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
                };
            }
        };

        if discover_val.is_null() || discover_val.is_undefined() {
            return run_legacy_probe(
                &ctx,
                plugin,
                &probe_fn,
                &base_ctx,
                &subs,
                &plugin_id,
                &display_name,
                &icon_url,
            );
        }

        // discoverAccounts is present — must be a function.
        let discover_fn: rquickjs::Function = match discover_val.into_function() {
            Some(f) => f,
            None => {
                return ProbeResult {
                    outputs: vec![error_output(
                        plugin,
                        "discoverAccounts must be a function".to_string(),
                    )],
                    subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
                };
            }
        };

        run_discovery_mode(
            &ctx,
            plugin,
            &probe_fn,
            &discover_fn,
            &base_ctx,
            &subs,
            &plugin_id,
            &display_name,
            &icon_url,
        )
    })
}

#[allow(clippy::too_many_arguments)]
/// Legacy mode: call probe once with a child context and default account.
fn run_legacy_probe<'js>(
    ctx: &Ctx<'js>,
    plugin: &LoadedPlugin,
    probe_fn: &rquickjs::Function<'js>,
    base_ctx: &Value<'js>,
    subs: &Arc<std::sync::Mutex<Vec<FileSubscription>>>,
    plugin_id: &str,
    display_name: &str,
    icon_url: &str,
) -> ProbeResult {
    // Create a child context with immutable default account.
    let account_ctx = match create_account_context(ctx, &AccountRef::default_account(), base_ctx) {
        Ok(c) => c,
        Err(_) => {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    "failed to create account context".to_string(),
                )],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
    };
    let result_value: Value = match probe_fn.call((account_ctx,)) {
        Ok(r) => r,
        Err(_) => {
            return ProbeResult {
                outputs: vec![error_output(plugin, extract_error_string(ctx))],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
    };

    let result = match resolve_js_result(ctx, result_value) {
        Ok(obj) => obj,
        Err(msg) => {
            return ProbeResult {
                outputs: vec![error_output(plugin, msg)],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
    };

    let output = build_plugin_output(&result, plugin_id, display_name, icon_url, None);

    ProbeResult {
        outputs: vec![output],
        subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
    }
}

#[allow(clippy::too_many_arguments)]
/// Discovery mode: call discoverAccounts, validate, then probe each account.
fn run_discovery_mode<'js>(
    ctx: &Ctx<'js>,
    plugin: &LoadedPlugin,
    probe_fn: &rquickjs::Function<'js>,
    discover_fn: &rquickjs::Function<'js>,
    base_ctx: &Value<'js>,
    subs: &Arc<std::sync::Mutex<Vec<FileSubscription>>>,
    plugin_id: &str,
    display_name: &str,
    icon_url: &str,
) -> ProbeResult {
    // Call discoverAccounts with the base context.
    let discovery_result_value: Value = match discover_fn.call((base_ctx.clone(),)) {
        Ok(r) => r,
        Err(_) => {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    format!("discoverAccounts failed: {}", extract_error_string(ctx)),
                )],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
    };

    let discovery_result = match resolve_js_result(ctx, discovery_result_value) {
        Ok(obj) => obj,
        Err(msg) => {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    format!("discoverAccounts failed: {}", msg),
                )],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
    };

    // Validate discovery result is an array.
    let accounts_array: Array = match discovery_result.into_array() {
        Some(arr) => arr,
        None => {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    "discoverAccounts must return an array".to_string(),
                )],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
    };

    // Parse and validate account descriptors.
    let accounts = match parse_discovery_accounts(&accounts_array) {
        Ok(accs) => accs,
        Err(msg) => {
            return ProbeResult {
                outputs: vec![error_output(plugin, msg)],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
    };

    // Empty valid list: produce zero outputs.
    if accounts.is_empty() {
        return ProbeResult {
            outputs: Vec::new(),
            subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
        };
    }

    // Probe each discovered account sequentially.
    let mut outputs = Vec::with_capacity(accounts.len());
    for account in &accounts {
        let account_ctx = match create_account_context(ctx, account, base_ctx) {
            Ok(c) => c,
            Err(_) => {
                outputs.push(error_output_with_account(
                    plugin,
                    "failed to create account context".to_string(),
                    account,
                ));
                continue;
            }
        };

        let result_value: Value = match probe_fn.call((account_ctx,)) {
            Ok(r) => r,
            Err(_) => {
                outputs.push(error_output_with_account(
                    plugin,
                    extract_error_string(ctx),
                    account,
                ));
                continue;
            }
        };

        let result = match resolve_js_result(ctx, result_value) {
            Ok(obj) => obj,
            Err(msg) => {
                outputs.push(error_output_with_account(plugin, msg, account));
                continue;
            }
        };

        // In discovery mode, identity is host-authoritative.
        let output = build_plugin_output(&result, plugin_id, display_name, icon_url, Some(account));
        outputs.push(output);
    }

    ProbeResult {
        outputs,
        subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
    }
}

/// Create a child context inheriting from `base_ctx` with an immutable
/// `account` property.
///
/// Uses `Object::new` + `set_prototype` for the prototype chain and
/// `Property` for immutable descriptors — no string interpolation of
/// untrusted account data and no dependency on `Object.freeze`.
fn create_account_context<'js>(
    ctx: &Ctx<'js>,
    account: &AccountRef,
    base_ctx: &Value<'js>,
) -> Result<Object<'js>, String> {
    // Build the account object with non-writable, non-configurable
    // properties using Property API — immune to monkey-patched Object.freeze.
    let acct =
        Object::new(ctx.clone()).map_err(|_| "failed to create account object".to_string())?;
    acct.prop("id", Property::from(account.id.as_str()).enumerable())
        .map_err(|_| "failed to set account id".to_string())?;
    acct.prop(
        "displayName",
        Property::from(account.display_name.as_str()).enumerable(),
    )
    .map_err(|_| "failed to set account displayName".to_string())?;

    // Create child context using Object::new + set_prototype.
    let child =
        Object::new(ctx.clone()).map_err(|_| "failed to create child context".to_string())?;

    // Convert base_ctx to Object for prototype chain.
    let base_obj = base_ctx
        .clone()
        .into_object()
        .ok_or_else(|| "base context is not an object".to_string())?;
    child
        .set_prototype(Some(&base_obj))
        .map_err(|_| "failed to set prototype".to_string())?;

    // Define immutable account property using Property API.
    child
        .prop("account", Property::from(acct).enumerable())
        .map_err(|_| "failed to define account property".to_string())?;

    Ok(child)
}

/// Parse and validate a discovery result array.
/// Returns up to 32 account descriptors with unique non-empty ids.
fn parse_discovery_accounts(array: &Array) -> Result<Vec<AccountRef>, String> {
    let len = array.len();
    if len > 32 {
        return Err(format!(
            "discoverAccounts returned {} accounts (max 32)",
            len
        ));
    }

    let mut accounts = Vec::with_capacity(len);
    let mut seen_ids = std::collections::HashSet::new();

    for idx in 0..len {
        let elem: Value = array
            .get(idx)
            .map_err(|_| format!("discoverAccounts: invalid element at index {}", idx))?;

        let obj = elem.into_object().ok_or_else(|| {
            format!(
                "discoverAccounts: element at index {} must be an object",
                idx
            )
        })?;

        let id: String = obj
            .get("id")
            .map_err(|_| format!("discoverAccounts: element at index {} missing id", idx))?;
        if id.trim().is_empty() {
            return Err(format!(
                "discoverAccounts: element at index {} has empty id",
                idx
            ));
        }

        let display_name: String = obj.get("displayName").map_err(|_| {
            format!(
                "discoverAccounts: element at index {} missing displayName",
                idx
            )
        })?;
        if display_name.trim().is_empty() {
            return Err(format!(
                "discoverAccounts: element at index {} has empty displayName",
                idx
            ));
        }

        if !seen_ids.insert(id.clone()) {
            return Err(format!(
                "discoverAccounts: duplicate account id '{}' at index {}",
                id, idx
            ));
        }

        accounts.push(AccountRef { id, display_name });
    }

    Ok(accounts)
}

/// Resolve a JS value that may be a sync result or a Promise.
fn resolve_js_result<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<Object<'js>, String> {
    if value.is_promise() {
        let promise = value
            .into_promise()
            .ok_or_else(|| "returned invalid promise".to_string())?;
        match promise.finish::<Object>() {
            Ok(obj) => Ok(obj),
            Err(Error::WouldBlock) => Err("returned unresolved promise".to_string()),
            Err(_) => Err(extract_error_string(ctx)),
        }
    } else {
        value
            .into_object()
            .ok_or_else(|| "returned non-object".to_string())
    }
}

/// Build a PluginOutput from a probe result object.
///
/// In legacy mode (`discovery_account` is None), existing account parsing
/// applies (absent/null/undefined -> default; present -> validated).
///
/// In discovery mode (`discovery_account` is Some), identity is host-authoritative:
/// absent/null/undefined/explicit default result.account means the discovered
/// account; a returned account that differs in either id or displayName produces
/// an account-specific error carrying the discovered identity.
fn build_plugin_output(
    result: &Object,
    plugin_id: &str,
    display_name: &str,
    icon_url: &str,
    discovery_account: Option<&AccountRef>,
) -> PluginOutput {
    let plan: Option<String> = result
        .get::<_, String>("plan")
        .ok()
        .filter(|s| !s.is_empty());

    let lines = match parse_lines(result) {
        Ok(lines) if !lines.is_empty() => lines,
        Ok(_) => vec![error_line("no lines returned".to_string())],
        Err(msg) => vec![error_line(msg)],
    };

    let account = match discovery_account {
        None => {
            // Legacy mode: existing behavior.
            match parse_account(result) {
                Ok(account) => account,
                Err(msg) => {
                    return PluginOutput {
                        provider_id: plugin_id.to_string(),
                        display_name: display_name.to_string(),
                        plan: None,
                        lines: vec![error_line(msg)],
                        icon_url: icon_url.to_string(),
                        account: AccountRef::default_account(),
                    };
                }
            }
        }
        Some(discovered) => {
            // Discovery mode: host-authoritative identity.
            match parse_account_in_discovery(result, discovered) {
                Ok(account) => account,
                Err(msg) => {
                    return PluginOutput {
                        provider_id: plugin_id.to_string(),
                        display_name: display_name.to_string(),
                        plan: None,
                        lines: vec![error_line(msg)],
                        icon_url: icon_url.to_string(),
                        account: discovered.clone(),
                    };
                }
            }
        }
    };

    PluginOutput {
        provider_id: plugin_id.to_string(),
        display_name: display_name.to_string(),
        plan,
        lines,
        icon_url: icon_url.to_string(),
        account,
    }
}

/// Parse account from a probe result in discovery mode.
/// Host-authoritative: absent/null/undefined -> discovered account.
/// An explicit `{id:"default",displayName:"default"}` is also treated as
/// unspecified and receives the discovered identity.
/// A genuinely mismatched id or displayName produces an account-specific
/// error with a generic mismatch message.
fn parse_account_in_discovery(
    result: &Object,
    discovered: &AccountRef,
) -> Result<AccountRef, String> {
    let has_account = match result.contains_key("account") {
        Ok(v) => v,
        Err(_) => {
            return Err("plugin returned invalid account: accessor threw".to_string());
        }
    };

    if !has_account {
        return Ok(discovered.clone());
    }

    let account_val: rquickjs::Value = match result.get("account") {
        Ok(v) => v,
        Err(_) => {
            return Err("plugin returned invalid account: accessor threw".to_string());
        }
    };

    if account_val.is_null() || account_val.is_undefined() {
        return Ok(discovered.clone());
    }

    let account_obj = match account_val.into_object() {
        Some(obj) => obj,
        None => {
            return Err(
                "plugin returned account that does not match discovered identity".to_string(),
            );
        }
    };

    let id: String = match account_obj.get("id") {
        Ok(v) => v,
        Err(_) => {
            return Err(
                "plugin returned account that does not match discovered identity".to_string(),
            );
        }
    };
    let display_name: String = match account_obj.get("displayName") {
        Ok(v) => v,
        Err(_) => {
            return Err(
                "plugin returned account that does not match discovered identity".to_string(),
            );
        }
    };

    // Explicit default is equivalent to unspecified — apply discovered identity.
    if id == "default" && display_name == "default" {
        return Ok(discovered.clone());
    }

    if id == discovered.id && display_name == discovered.display_name {
        Ok(discovered.clone())
    } else {
        Err("plugin returned account that does not match discovered identity".to_string())
    }
}

fn error_output_with_account(
    plugin: &LoadedPlugin,
    message: String,
    account: &AccountRef,
) -> PluginOutput {
    PluginOutput {
        provider_id: plugin.manifest.id.clone(),
        display_name: plugin.manifest.name.clone(),
        plan: None,
        lines: vec![error_line(message)],
        icon_url: plugin.icon_data_url.clone(),
        account: account.clone(),
    }
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
            var originalDiscoverAccounts = typeof plugin.discoverAccounts === "function"
                ? plugin.discoverAccounts.bind(plugin)
                : null;

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
                }},
                originalDiscoverAccounts: originalDiscoverAccounts,
                replaceDiscoverAccounts: function(replacement) {{
                    if (typeof replacement !== "function") {{
                        throw "replaceDiscoverAccounts expects a function";
                    }}
                    plugin.discoverAccounts = function(ctx) {{
                        return replacement(ctx, originalDiscoverAccounts);
                    }};
                    return plugin.discoverAccounts;
                }},
                wrapDiscoverAccounts: function(wrapper) {{
                    if (typeof wrapper !== "function") {{
                        throw "wrapDiscoverAccounts expects a function";
                    }}
                    var currentDiscoverAccounts = typeof plugin.discoverAccounts === "function"
                        ? plugin.discoverAccounts.bind(plugin)
                        : null;
                    if (!currentDiscoverAccounts) {{
                        throw "wrapDiscoverAccounts requires a current discoverAccounts function";
                    }}
                    plugin.discoverAccounts = function(ctx) {{
                        return wrapper(ctx, currentDiscoverAccounts, originalDiscoverAccounts);
                    }};
                    return plugin.discoverAccounts;
                }},
                resetDiscoverAccounts: function() {{
                    if (originalDiscoverAccounts) {{
                        plugin.discoverAccounts = originalDiscoverAccounts;
                    }} else {{
                        delete plugin.discoverAccounts;
                    }}
                    return typeof plugin.discoverAccounts === "function"
                        ? plugin.discoverAccounts
                        : null;
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

fn parse_account(result: &Object) -> Result<AccountRef, String> {
    // Check whether account property exists on the result object.
    // If the accessor throws (e.g. a Proxy trap), return a validation error.
    let has_account = match result.contains_key("account") {
        Ok(v) => v,
        Err(_) => {
            return Err("plugin returned invalid account: accessor threw".to_string());
        }
    };

    if !has_account {
        return Ok(AccountRef::default_account());
    }

    // Property exists — read it. A getter that throws here is also an error.
    let account_val: rquickjs::Value = match result.get("account") {
        Ok(v) => v,
        Err(_) => {
            return Err("plugin returned invalid account: accessor threw".to_string());
        }
    };

    if account_val.is_null() || account_val.is_undefined() {
        return Ok(AccountRef::default_account());
    }

    let account_obj = account_val
        .into_object()
        .ok_or_else(|| "plugin returned invalid account: must be an object".to_string())?;

    let id: String = account_obj
        .get("id")
        .map_err(|_| "plugin returned account without id".to_string())?;
    if id.trim().is_empty() {
        return Err("plugin returned account with empty id".to_string());
    }

    let display_name: String = account_obj
        .get("displayName")
        .map_err(|_| "plugin returned account without displayName".to_string())?;
    if display_name.trim().is_empty() {
        return Err("plugin returned account with empty displayName".to_string());
    }

    Ok(AccountRef { id, display_name })
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
        account: AccountRef::default_account(),
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

    fn error_text(output: &PluginOutput) -> String {
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
        let result = run_probe(&plugin, &temp_app_dir("sync"), "0.0.0", None);
        assert_eq!(error_text(&result.outputs[0]), "boom");
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
        let result = run_probe(&plugin, &temp_app_dir("async"), "0.0.0", None);
        assert_eq!(error_text(&result.outputs[0]), "boom");
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

        let result = run_probe(
            &plugin,
            &app_data_dir,
            "0.0.0",
            Some(overrides_dir.as_path()),
        );

        let has_override_badge = result.outputs[0].lines.iter().any(|line| {
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

    #[test]
    fn run_probe_collects_subscriptions_from_subscribe_file() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    ctx.host.fs.subscribeFile("/tmp/test-dep-a.json");
                    ctx.host.fs.subscribeFile("/tmp/test-dep-b.json");
                    return {
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("subs"), "0.0.0", None);

        // Should have collected the two subscriptions.
        assert_eq!(result.subscriptions.len(), 2, "expected 2 subscriptions");
        let paths: Vec<String> = result
            .subscriptions
            .iter()
            .map(|p| p.path.to_string_lossy().to_string())
            .collect();
        assert!(paths.iter().any(|p| p.ends_with("test-dep-a.json")));
        assert!(paths.iter().any(|p| p.ends_with("test-dep-b.json")));

        // Output should be the normal success result.
        assert_eq!(result.outputs[0].lines.len(), 1);
    }

    #[test]
    fn run_probe_subscriptions_are_empty_when_no_subscribe_file() {
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
        let result = run_probe(&plugin, &temp_app_dir("no-subs"), "0.0.0", None);
        assert!(
            result.subscriptions.is_empty(),
            "expected no subscriptions when subscribeFile is never called"
        );
        assert_eq!(result.outputs[0].lines.len(), 1);
        assert_eq!(result.outputs[0].lines.len(), 1);
    }

    #[test]
    fn run_probe_collects_subscriptions_from_entry_script_before_probe() {
        // Subscriptions declared during entry script evaluation (before
        // probe() is called) must be collected in the result.
        let plugin = test_plugin(
            r#"
            // Declare dependency at module level, before probe is defined.
            var ctx = globalThis.__openusage_ctx;
            ctx.host.fs.subscribeFile("/tmp/entry-dep.json");

            globalThis.__openusage_plugin = {
                probe(ctx) {
                    // Also declare one during probe.
                    ctx.host.fs.subscribeFile("/tmp/probe-dep.json");
                    return {
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("entry-subs"), "0.0.0", None);

        // Should have collected subscriptions from both entry script and probe.
        assert_eq!(result.subscriptions.len(), 2, "expected 2 subscriptions");
        let paths: Vec<String> = result
            .subscriptions
            .iter()
            .map(|p| p.path.to_string_lossy().to_string())
            .collect();
        assert!(
            paths.iter().any(|p| p.ends_with("entry-dep.json")),
            "expected entry-dep.json in subscriptions"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("probe-dep.json")),
            "expected probe-dep.json in subscriptions"
        );

        // Output should be the normal success result.
        assert_eq!(result.outputs[0].lines.len(), 1);
    }

    #[test]
    fn run_probe_retains_subscriptions_when_probe_throws_sync() {
        // When probe() throws synchronously, subscriptions collected before
        // the throw must still be present in ProbeResult.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    ctx.host.fs.subscribeFile("/tmp/error-dep.json");
                    throw "sync-error";
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("error-sync"), "0.0.0", None);

        // Should have collected the subscription despite the error.
        assert_eq!(result.subscriptions.len(), 1, "expected 1 subscription");
        assert!(
            result.subscriptions[0]
                .path
                .to_string_lossy()
                .ends_with("error-dep.json"),
            "expected error-dep.json in subscriptions"
        );

        // Output should be an error badge.
        assert_eq!(result.outputs[0].lines.len(), 1);
        match &result.outputs[0].lines[0] {
            MetricLine::Badge { label, text, .. } => {
                assert_eq!(label, "Error");
                assert_eq!(text, "sync-error");
            }
            other => panic!("expected error badge, got {:?}", other),
        }
    }

    #[test]
    fn run_probe_retains_subscriptions_when_probe_throws_async() {
        // When probe() rejects asynchronously, subscriptions collected before
        // the throw must still be present in ProbeResult.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe: async function(ctx) {
                    ctx.host.fs.subscribeFile("/tmp/async-error-dep.json");
                    throw "async-error";
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("error-async"), "0.0.0", None);

        // Should have collected the subscription despite the error.
        assert_eq!(result.subscriptions.len(), 1, "expected 1 subscription");
        assert!(
            result.subscriptions[0]
                .path
                .to_string_lossy()
                .ends_with("async-error-dep.json"),
            "expected async-error-dep.json in subscriptions"
        );

        // Output should be an error badge.
        assert_eq!(result.outputs[0].lines.len(), 1);
        match &result.outputs[0].lines[0] {
            MetricLine::Badge { label, text, .. } => {
                assert_eq!(label, "Error");
                assert_eq!(text, "async-error");
            }
            other => panic!("expected error badge, got {:?}", other),
        }
    }

    #[test]
    fn run_probe_collects_subscriptions_from_entry_script_and_override() {
        // When both the entry script and the override declare dependencies,
        // the result must contain subscriptions from both sources.
        let plugin = test_plugin(
            r#"
            // Entry script declares a dependency.
            var ctx = globalThis.__openusage_ctx;
            ctx.host.fs.subscribeFile("/tmp/entry-dep.json");

            globalThis.__openusage_plugin = {
                probe(ctx) {
                    ctx.host.fs.subscribeFile("/tmp/probe-dep.json");
                    return {
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );

        let app_data_dir = temp_app_dir("entry-override-subs");
        let overrides_dir = temp_app_dir("entry-override-dir");
        std::fs::create_dir_all(&overrides_dir).expect("create overrides dir");
        std::fs::write(
            overrides_dir.join("test.js"),
            r#"
            // Override declares a dependency before wrapping.
            globalThis.__openusage_ctx.host.fs.subscribeFile("/tmp/override-dep.json");

            globalThis.__openusage_override.wrapProbe(function(ctx, currentProbe, originalProbe) {
                return originalProbe(ctx);
            });
            "#,
        )
        .expect("write override script");

        let result = run_probe(
            &plugin,
            &app_data_dir,
            "0.0.0",
            Some(overrides_dir.as_path()),
        );

        // Should have collected subscriptions from entry script, probe, and override.
        assert_eq!(result.subscriptions.len(), 3, "expected 3 subscriptions");
        let paths: Vec<String> = result
            .subscriptions
            .iter()
            .map(|p| p.path.to_string_lossy().to_string())
            .collect();
        assert!(
            paths.iter().any(|p| p.ends_with("entry-dep.json")),
            "expected entry-dep.json"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("probe-dep.json")),
            "expected probe-dep.json"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("override-dep.json")),
            "expected override-dep.json"
        );

        // Output should be the normal success result.
        assert_eq!(result.outputs[0].lines.len(), 1);
    }

    #[test]
    fn run_probe_collects_subscriptions_with_existence_metadata() {
        // Use a temp directory with one explicitly created file and one
        // missing sibling.  Assert both true and false metadata values.
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
        let result = run_probe(&plugin, &temp_app_dir("existence-meta"), "0.0.0", None);

        assert_eq!(result.subscriptions.len(), 2, "expected 2 subscriptions");

        let existing_sub = result
            .subscriptions
            .iter()
            .find(|s| s.path == existing_path)
            .expect("expected present.json subscription");
        assert!(
            existing_sub.existed_at_declaration,
            "present.json exists, so existed_at_declaration should be true"
        );

        let missing_sub = result
            .subscriptions
            .iter()
            .find(|s| s.path == missing_path)
            .expect("expected absent.json subscription");
        assert!(
            !missing_sub.existed_at_declaration,
            "absent.json does not exist, so existed_at_declaration should be false"
        );
    }

    #[test]
    fn account_ref_default_serializes_camel_case() {
        let account = AccountRef::default_account();
        let json: JsonValue = serde_json::to_value(&account).expect("serialize");
        let obj = json.as_object().expect("object");
        assert_eq!(obj.get("id").and_then(|v| v.as_str()), Some("default"));
        assert_eq!(
            obj.get("displayName").and_then(|v| v.as_str()),
            Some("default")
        );
        assert!(
            obj.get("display_name").is_none(),
            "should not have snake_case key"
        );
    }

    #[test]
    fn plugin_output_success_has_default_account() {
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
        let result = run_probe(&plugin, &temp_app_dir("success-account"), "0.0.0", None);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn plugin_output_error_has_default_account() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    throw "boom";
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("error-account"), "0.0.0", None);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn error_output_has_default_account() {
        let plugin = test_plugin("");
        let output = error_output(&plugin, "test error".to_string());
        assert_eq!(output.account, AccountRef::default_account());
    }

    #[test]
    fn plugin_output_deserializes_pre_account_json_with_default_account() {
        // A pre-account PluginOutput JSON payload (no "account" key)
        // must deserialize with the default account via #[serde(default)].
        let json = r#"{
            "providerId": "test-provider",
            "displayName": "Test",
            "plan": null,
            "lines": [],
            "iconUrl": "data:image/svg+xml;base64,"
        }"#;

        let output: PluginOutput =
            serde_json::from_str(json).expect("deserialize pre-account json");
        assert_eq!(output.account, AccountRef::default_account());
        assert_eq!(output.provider_id, "test-provider");
    }

    #[test]
    fn plugin_output_custom_account_preserved() {
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
        let result = run_probe(&plugin, &temp_app_dir("custom-account"), "0.0.0", None);
        assert_eq!(
            result.outputs[0].account,
            AccountRef {
                id: "custom-id".to_string(),
                display_name: "Custom Name".to_string(),
            }
        );
    }

    #[test]
    fn plugin_output_omitted_account_defaults() {
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
        let result = run_probe(&plugin, &temp_app_dir("omit-account"), "0.0.0", None);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn plugin_output_null_account_defaults() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: null,
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("null-account"), "0.0.0", None);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn plugin_output_invalid_account_non_object_errors() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: "not-an-object",
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("invalid-account"), "0.0.0", None);
        assert_eq!(
            result.outputs[0].account,
            AccountRef::default_account(),
            "invalid account must produce error output with default account"
        );
        assert_eq!(result.outputs[0].lines.len(), 1);
        match &result.outputs[0].lines[0] {
            MetricLine::Badge { label, text, .. } => {
                assert_eq!(label, "Error");
                assert!(
                    text.contains("invalid account"),
                    "error text should mention invalid account: {}",
                    text
                );
            }
            other => panic!("expected error badge, got {:?}", other),
        }
    }

    #[test]
    fn plugin_output_invalid_account_empty_id_errors() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { id: "", displayName: "Name" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("empty-id"), "0.0.0", None);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        match &result.outputs[0].lines[0] {
            MetricLine::Badge { text, .. } => {
                assert!(
                    text.contains("empty id"),
                    "error text should mention empty id: {}",
                    text
                );
            }
            other => panic!("expected error badge, got {:?}", other),
        }
    }

    #[test]
    fn plugin_output_invalid_account_empty_display_name_errors() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { id: "custom-id", displayName: "" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("empty-display-name"), "0.0.0", None);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        match &result.outputs[0].lines[0] {
            MetricLine::Badge { text, .. } => {
                assert!(
                    text.contains("empty displayName"),
                    "error text should mention empty displayName: {}",
                    text
                );
            }
            other => panic!("expected error badge, got {:?}", other),
        }
    }

    #[test]
    fn plugin_output_invalid_account_missing_id_errors() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { displayName: "Name" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("missing-id"), "0.0.0", None);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        match &result.outputs[0].lines[0] {
            MetricLine::Badge { text, .. } => {
                assert!(
                    text.contains("without id"),
                    "error text should mention missing id: {}",
                    text
                );
            }
            other => panic!("expected error badge, got {:?}", other),
        }
    }

    #[test]
    fn plugin_output_undefined_account_defaults() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: undefined,
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("undefined-account"), "0.0.0", None);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn plugin_output_invalid_account_missing_display_name_errors() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { id: "custom-id" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("missing-displayname"), "0.0.0", None);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        match &result.outputs[0].lines[0] {
            MetricLine::Badge { text, .. } => {
                assert!(
                    text.contains("without displayName"),
                    "error text should mention missing displayName: {}",
                    text
                );
            }
            other => panic!("expected error badge, got {:?}", other),
        }
    }

    #[test]
    fn plugin_output_throwing_account_accessor_errors() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    var result = {
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                    Object.defineProperty(result, "account", {
                        get: function() { throw "getter-boom"; },
                        enumerable: true,
                        configurable: true
                    });
                    return result;
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("throwing-accessor"), "0.0.0", None);
        assert_eq!(
            result.outputs[0].account,
            AccountRef::default_account(),
            "throwing accessor must produce error output with default account"
        );
        match &result.outputs[0].lines[0] {
            MetricLine::Badge { text, .. } => {
                assert!(
                    text.contains("accessor threw"),
                    "error text should mention accessor threw: {}",
                    text
                );
            }
            other => panic!("expected error badge, got {:?}", other),
        }
    }

    // ---------------------------------------------------------------------------
    // Synthetic override-context tests for discoverAccounts API
    // ---------------------------------------------------------------------------

    /// Helper: create a JS runtime, evaluate plugin script, inject override API,
    /// evaluate override script, then let the closure inspect the context.
    fn with_override_context<F>(plugin_script: &str, override_script: &str, plugin_id: &str, f: F)
    where
        F: FnOnce(&Ctx),
    {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            ctx.eval::<(), _>(plugin_script.as_bytes())
                .expect("plugin script eval");
            inject_override_api(&ctx, plugin_id).expect("override api injection");
            if !override_script.is_empty() {
                ctx.eval::<(), _>(override_script.as_bytes())
                    .expect("override script eval");
            }
            f(&ctx);
        });
    }

    #[test]
    fn override_replace_discover_accounts_adds_to_legacy_plugin() {
        // A legacy plugin with only probe() gains discoverAccounts via replace.
        with_override_context(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                }
            };
            "#,
            r#"
            var ov = globalThis.__openusage_override;
            var plugin = globalThis.__openusage_plugin;
            globalThis.__test_result = {};

            // originalDiscoverAccounts must be null for a legacy plugin
            globalThis.__test_result.originalIsNull = (ov.originalDiscoverAccounts === null);

            // replaceDiscoverAccounts adds the function
            var ret = ov.replaceDiscoverAccounts(function(ctx, original) {
                return [{ id: "work", displayName: "Work Account" }];
            });
            globalThis.__test_result.retIsFunction = (typeof ret === "function");
            globalThis.__test_result.pluginHasDiscovery = (typeof plugin.discoverAccounts === "function");

            // Calling the discovery function returns the expected accounts
            var accounts = plugin.discoverAccounts(null);
            globalThis.__test_result.accountsJson = JSON.stringify(accounts);
            globalThis.__test_result.accountsLength = accounts.length;
            globalThis.__test_result.firstAccountId = accounts[0].id;
            "#,
            "test",
            |ctx| {
                let globals = ctx.globals();
                let result: Object = globals.get("__test_result").expect("test result");
                assert!(
                    result
                        .get::<_, bool>("originalIsNull")
                        .expect("originalIsNull")
                );
                assert!(
                    result
                        .get::<_, bool>("retIsFunction")
                        .expect("retIsFunction")
                );
                assert!(
                    result
                        .get::<_, bool>("pluginHasDiscovery")
                        .expect("pluginHasDiscovery")
                );
                assert_eq!(
                    result
                        .get::<_, i32>("accountsLength")
                        .expect("accountsLength"),
                    1
                );
                assert_eq!(
                    result
                        .get::<_, String>("firstAccountId")
                        .expect("firstAccountId"),
                    "work"
                );
            },
        );
    }

    #[test]
    fn override_wrap_discover_accounts_appends_to_native() {
        // A plugin with native discoverAccounts gets wrapped to append accounts.
        with_override_context(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return [{ id: "native", displayName: "Native Account" }];
                }
            };
            "#,
            r#"
            var ov = globalThis.__openusage_override;
            var plugin = globalThis.__openusage_plugin;
            globalThis.__test_result = {};

            // originalDiscoverAccounts must be the native function
            globalThis.__test_result.originalIsFunction = (typeof ov.originalDiscoverAccounts === "function");

            // wrapDiscoverAccounts chains
            var ret = ov.wrapDiscoverAccounts(function(ctx, current, original) {
                var accounts = current(ctx);
                accounts.push({ id: "wrapped", displayName: "Wrapped Account" });
                return accounts;
            });
            globalThis.__test_result.retIsFunction = (typeof ret === "function");

            // Calling the wrapped function returns both accounts
            var accounts = plugin.discoverAccounts(null);
            globalThis.__test_result.accountsJson = JSON.stringify(accounts);
            globalThis.__test_result.accountsLength = accounts.length;
            globalThis.__test_result.hasNative = accounts.some(function(a) { return a.id === "native"; });
            globalThis.__test_result.hasWrapped = accounts.some(function(a) { return a.id === "wrapped"; });
            "#,
            "test",
            |ctx| {
                let globals = ctx.globals();
                let result: Object = globals.get("__test_result").expect("test result");
                assert!(
                    result
                        .get::<_, bool>("originalIsFunction")
                        .expect("originalIsFunction")
                );
                assert!(
                    result
                        .get::<_, bool>("retIsFunction")
                        .expect("retIsFunction")
                );
                assert_eq!(
                    result
                        .get::<_, i32>("accountsLength")
                        .expect("accountsLength"),
                    2
                );
                assert!(result.get::<_, bool>("hasNative").expect("hasNative"));
                assert!(result.get::<_, bool>("hasWrapped").expect("hasWrapped"));
            },
        );
    }

    #[test]
    fn override_reset_discover_accounts_restores_native() {
        // After wrapping, reset restores the original native discoverAccounts.
        with_override_context(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return [{ id: "native", displayName: "Native Account" }];
                }
            };
            "#,
            r#"
            var ov = globalThis.__openusage_override;
            var plugin = globalThis.__openusage_plugin;
            globalThis.__test_result = {};

            // Wrap first
            ov.wrapDiscoverAccounts(function(ctx, current, original) {
                var accounts = current(ctx);
                accounts.push({ id: "extra", displayName: "Extra" });
                return accounts;
            });
            globalThis.__test_result.afterWrapIsWrapped = (typeof plugin.discoverAccounts === "function");

            // Reset
            var ret = ov.resetDiscoverAccounts();
            globalThis.__test_result.resetRetIsFunction = (typeof ret === "function");
            globalThis.__test_result.afterResetIsFunction = (typeof plugin.discoverAccounts === "function");

            // Calling after reset returns only the native account
            var accounts = plugin.discoverAccounts(null);
            globalThis.__test_result.accountsLength = accounts.length;
            globalThis.__test_result.firstAccountId = accounts[0].id;
            "#,
            "test",
            |ctx| {
                let globals = ctx.globals();
                let result: Object = globals.get("__test_result").expect("test result");
                assert!(
                    result
                        .get::<_, bool>("afterWrapIsWrapped")
                        .expect("afterWrapIsWrapped")
                );
                assert!(
                    result
                        .get::<_, bool>("resetRetIsFunction")
                        .expect("resetRetIsFunction")
                );
                assert!(
                    result
                        .get::<_, bool>("afterResetIsFunction")
                        .expect("afterResetIsFunction")
                );
                assert_eq!(
                    result
                        .get::<_, i32>("accountsLength")
                        .expect("accountsLength"),
                    1
                );
                assert_eq!(
                    result
                        .get::<_, String>("firstAccountId")
                        .expect("firstAccountId"),
                    "native"
                );
            },
        );
    }

    #[test]
    fn override_reset_discover_accounts_removes_override_added() {
        // When replace added discovery to a legacy plugin, reset removes it.
        with_override_context(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                }
            };
            "#,
            r#"
            var ov = globalThis.__openusage_override;
            var plugin = globalThis.__openusage_plugin;
            globalThis.__test_result = {};

            // Add discovery via replace
            ov.replaceDiscoverAccounts(function(ctx, original) {
                return [{ id: "added", displayName: "Added" }];
            });
            globalThis.__test_result.afterReplaceIsFunction = (typeof plugin.discoverAccounts === "function");

            // Reset removes it
            var ret = ov.resetDiscoverAccounts();
            globalThis.__test_result.resetRetIsNull = (ret === null);
            globalThis.__test_result.afterResetIsUndefined = (typeof plugin.discoverAccounts === "undefined");
            "#,
            "test",
            |ctx| {
                let globals = ctx.globals();
                let result: Object = globals.get("__test_result").expect("test result");
                assert!(
                    result
                        .get::<_, bool>("afterReplaceIsFunction")
                        .expect("afterReplaceIsFunction")
                );
                assert!(
                    result
                        .get::<_, bool>("resetRetIsNull")
                        .expect("resetRetIsNull")
                );
                assert!(
                    result
                        .get::<_, bool>("afterResetIsUndefined")
                        .expect("afterResetIsUndefined")
                );
            },
        );
    }

    #[test]
    fn override_wrap_discover_accounts_without_current_fails() {
        // wrapDiscoverAccounts on a plugin without discoverAccounts must throw.
        with_override_context(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                }
            };
            "#,
            r#"
            var ov = globalThis.__openusage_override;
            globalThis.__test_result = {};
            try {
                ov.wrapDiscoverAccounts(function(ctx, current, original) {
                    return current(ctx);
                });
                globalThis.__test_result.didThrow = false;
            } catch (e) {
                globalThis.__test_result.didThrow = true;
                globalThis.__test_result.errorMsg = String(e);
            }
            "#,
            "test",
            |ctx| {
                let globals = ctx.globals();
                let result: Object = globals.get("__test_result").expect("test result");
                assert!(
                    result.get::<_, bool>("didThrow").expect("didThrow"),
                    "wrapDiscoverAccounts should throw when no current discovery function exists"
                );
                let msg: String = result.get("errorMsg").expect("errorMsg");
                assert!(
                    msg.contains("requires a current"),
                    "error message should mention requirement: {}",
                    msg
                );
            },
        );
    }

    #[test]
    fn override_discover_accounts_does_not_affect_probe_override() {
        // Existing probe override behavior must remain unchanged when
        // discovery override API is present.
        with_override_context(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        lines: [ctx.line.text({ label: "Status", value: "original" })]
                    };
                }
            };
            "#,
            r#"
            var ov = globalThis.__openusage_override;
            var plugin = globalThis.__openusage_plugin;
            globalThis.__test_result = {};

            // Probe API still works: replaceProbe
            var replaced = ov.replaceProbe(function(ctx, original) {
                var result = original(ctx);
                result.lines.push({ type: "text", label: "Override", value: "replaced" });
                return result;
            });
            globalThis.__test_result.replaceProbeIsFunction = (typeof replaced === "function");

            // wrapProbe still works
            var wrapped = ov.wrapProbe(function(ctx, current, original) {
                var result = current(ctx);
                result.lines.push({ type: "text", label: "Override", value: "wrapped" });
                return result;
            });
            globalThis.__test_result.wrapProbeIsFunction = (typeof wrapped === "function");

            // resetProbe still works
            var reset = ov.resetProbe();
            globalThis.__test_result.resetProbeIsFunction = (typeof reset === "function");

            // originalProbe is still accessible
            globalThis.__test_result.originalProbeIsFunction = (typeof ov.originalProbe === "function");

            // Discovery API is also present
            globalThis.__test_result.hasOriginalDiscoverAccounts = ("originalDiscoverAccounts" in ov);
            globalThis.__test_result.hasReplaceDiscoverAccounts = (typeof ov.replaceDiscoverAccounts === "function");
            globalThis.__test_result.hasWrapDiscoverAccounts = (typeof ov.wrapDiscoverAccounts === "function");
            globalThis.__test_result.hasResetDiscoverAccounts = (typeof ov.resetDiscoverAccounts === "function");
            "#,
            "test",
            |ctx| {
                let globals = ctx.globals();
                let result: Object = globals.get("__test_result").expect("test result");
                assert!(
                    result
                        .get::<_, bool>("replaceProbeIsFunction")
                        .expect("replaceProbeIsFunction")
                );
                assert!(
                    result
                        .get::<_, bool>("wrapProbeIsFunction")
                        .expect("wrapProbeIsFunction")
                );
                assert!(
                    result
                        .get::<_, bool>("resetProbeIsFunction")
                        .expect("resetProbeIsFunction")
                );
                assert!(
                    result
                        .get::<_, bool>("originalProbeIsFunction")
                        .expect("originalProbeIsFunction")
                );
                assert!(
                    result
                        .get::<_, bool>("hasOriginalDiscoverAccounts")
                        .expect("hasOriginalDiscoverAccounts")
                );
                assert!(
                    result
                        .get::<_, bool>("hasReplaceDiscoverAccounts")
                        .expect("hasReplaceDiscoverAccounts")
                );
                assert!(
                    result
                        .get::<_, bool>("hasWrapDiscoverAccounts")
                        .expect("hasWrapDiscoverAccounts")
                );
                assert!(
                    result
                        .get::<_, bool>("hasResetDiscoverAccounts")
                        .expect("hasResetDiscoverAccounts")
                );
            },
        );
    }
    // ---------------------------------------------------------------------------
    // Discovery execution tests (Phase 2): run_probe with discoverAccounts
    // ---------------------------------------------------------------------------

    #[test]
    fn legacy_probe_has_immutable_default_account_context() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    var acct = ctx.account;
                    var id = acct.id;
                    var displayName = acct.displayName;
                    var stillDefault = ctx.account.id === "default";
                    var desc = Object.getOwnPropertyDescriptor(acct, "id");
                    return {
                        lines: [
                            ctx.line.text({ label: "ID", value: id }),
                            ctx.line.text({ label: "DisplayName", value: displayName }),
                            ctx.line.text({ label: "StillDefault", value: String(stillDefault) }),
                            ctx.line.text({ label: "Writable", value: String(desc.writable) }),
                            ctx.line.text({ label: "Configurable", value: String(desc.configurable) })
                        ]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("legacy-immutable"), "0.0.0", None);
        let lines = &result.outputs[0].lines;
        let get_text = |label: &str| -> String {
            lines
                .iter()
                .find_map(|l| match l {
                    MetricLine::Text {
                        label: l, value, ..
                    } if l == label => Some(value.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        assert_eq!(get_text("ID"), "default");
        assert_eq!(get_text("DisplayName"), "default");
        assert_eq!(get_text("StillDefault"), "true");
        assert_eq!(
            get_text("Writable"),
            "false",
            "account.id must be non-writable"
        );
        assert_eq!(
            get_text("Configurable"),
            "false",
            "account.id must be non-configurable"
        );
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn discovery_two_accounts_ordered_distinct_contexts() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { id: ctx.account.id, displayName: ctx.account.displayName },
                        lines: [ctx.line.text({ label: "Account", value: ctx.account.id })]
                    };
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
        let result = run_probe(&plugin, &temp_app_dir("two-accounts"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 2);
        assert_eq!(result.outputs[0].account.id, "work");
        assert_eq!(result.outputs[1].account.id, "personal");
    }

    #[test]
    fn discovery_host_authoritative_identity() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    if (ctx.account.id === "work") {
                        return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                    }
                    return {
                        account: { id: "personal", displayName: "Personal" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
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
        let result = run_probe(&plugin, &temp_app_dir("host-auth"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 2);
        assert_eq!(result.outputs[0].account.id, "work");
        assert_eq!(result.outputs[1].account.id, "personal");
    }

    #[test]
    fn discovery_async_supported() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Account", value: ctx.account.id })] };
                },
                async discoverAccounts(ctx) {
                    return [{ id: "async-acc", displayName: "Async Account" }];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("async-disc"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account.id, "async-acc");
    }

    #[test]
    fn discovery_empty_list_produces_zero_outputs() {
        let plugin = test_plugin(
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
        let result = run_probe(&plugin, &temp_app_dir("empty-disc"), "0.0.0", None);
        assert!(
            result.outputs.is_empty(),
            "empty discovery should produce zero outputs"
        );
    }

    #[test]
    fn discovery_non_array_returns_error() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return "not-an-array";
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("non-array"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        let err = error_text(&result.outputs[0]);
        assert!(
            err.contains("returned non-object"),
            "expected non-object error, got: {}",
            err
        );
    }

    #[test]
    fn discovery_missing_id_returns_error() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return [{ displayName: "No ID" }];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("missing-id"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        assert!(error_text(&result.outputs[0]).contains("missing id"));
    }

    #[test]
    fn discovery_empty_id_returns_error() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return [{ id: "", displayName: "Empty" }];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("empty-id"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn discovery_empty_display_name_returns_error() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return [{ id: "x", displayName: "" }];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("empty-dn"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn discovery_duplicate_ids_returns_error() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return [
                        { id: "dup", displayName: "First" },
                        { id: "dup", displayName: "Second" }
                    ];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("dup-ids"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        assert!(error_text(&result.outputs[0]).contains("duplicate"));
    }

    #[test]
    fn discovery_limit_exceeded_returns_error() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    var arr = [];
                    for (var i = 0; i < 33; i++) {
                        arr.push({ id: "a" + i, displayName: "A" + i });
                    }
                    return arr;
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("limit-exceeded"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        assert!(error_text(&result.outputs[0]).contains("max 32"));
    }

    #[test]
    fn discovery_exception_returns_error() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    throw "discovery-boom";
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("disc-exception"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        assert!(error_text(&result.outputs[0]).contains("discovery-boom"));
    }

    #[test]
    fn discovery_non_function_error() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts: "not-a-function"
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("non-fn"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        assert!(error_text(&result.outputs[0]).contains("must be a function"));
    }

    #[test]
    fn discovery_one_account_failure_does_not_suppress_another() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    if (ctx.account.id === "broken") {
                        throw "broken-account-boom";
                    }
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return [
                        { id: "good", displayName: "Good" },
                        { id: "broken", displayName: "Broken" },
                        { id: "also-good", displayName: "Also Good" }
                    ];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("partial-fail"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 3);
        assert_eq!(result.outputs[0].account.id, "good");
        assert!(result.outputs[0].lines[0].to_string().contains("ok"));
        assert_eq!(result.outputs[1].account.id, "broken");
        assert!(
            result.outputs[1].lines[0]
                .to_string()
                .contains("broken-account-boom")
        );
        assert_eq!(result.outputs[2].account.id, "also-good");
        assert!(result.outputs[2].lines[0].to_string().contains("ok"));
    }

    #[test]
    fn discovery_subscriptions_are_unioned() {
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
        let result = run_probe(&plugin, &temp_app_dir("union-subs"), "0.0.0", None);
        assert_eq!(result.subscriptions.len(), 3);
        let paths: Vec<String> = result
            .subscriptions
            .iter()
            .map(|p| p.path.to_string_lossy().to_string())
            .collect();
        assert!(paths.iter().any(|p| p.ends_with("discovery-dep.json")));
        assert!(paths.iter().any(|p| p.ends_with("probe-a.json")));
        assert!(paths.iter().any(|p| p.ends_with("probe-b.json")));
        assert_eq!(result.outputs.len(), 2);
    }

    #[test]
    fn discovery_override_replace_affects_runtime() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Account", value: ctx.account.id })] };
                }
            };
            "#,
        );
        let app_data_dir = temp_app_dir("ov-replace-disc");
        let overrides_dir = temp_app_dir("ov-replace-disc-dir");
        std::fs::create_dir_all(&overrides_dir).expect("create overrides dir");
        std::fs::write(
            overrides_dir.join("test.js"),
            r#"
            globalThis.__openusage_override.replaceDiscoverAccounts(function(ctx, original) {
                return [{ id: "override-acc", displayName: "Override Account" }];
            });
            "#,
        )
        .expect("write override script");
        let result = run_probe(
            &plugin,
            &app_data_dir,
            "0.0.0",
            Some(overrides_dir.as_path()),
        );
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account.id, "override-acc");
    }

    #[test]
    fn discovery_override_wrap_affects_runtime() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Account", value: ctx.account.id })] };
                },
                discoverAccounts(ctx) {
                    return [{ id: "native", displayName: "Native" }];
                }
            };
            "#,
        );
        let app_data_dir = temp_app_dir("ov-wrap-disc");
        let overrides_dir = temp_app_dir("ov-wrap-disc-dir");
        std::fs::create_dir_all(&overrides_dir).expect("create overrides dir");
        std::fs::write(
            overrides_dir.join("test.js"),
            r#"
            globalThis.__openusage_override.wrapDiscoverAccounts(function(ctx, current, original) {
                var accounts = current(ctx);
                accounts.push({ id: "wrapped", displayName: "Wrapped" });
                return accounts;
            });
            "#,
        )
        .expect("write override script");
        let result = run_probe(
            &plugin,
            &app_data_dir,
            "0.0.0",
            Some(overrides_dir.as_path()),
        );
        assert_eq!(result.outputs.len(), 2);
        assert_eq!(result.outputs[0].account.id, "native");
        assert_eq!(result.outputs[1].account.id, "wrapped");
    }

    #[test]
    fn discovery_32_accounts_is_valid() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Account", value: ctx.account.id })] };
                },
                discoverAccounts(ctx) {
                    var arr = [];
                    for (var i = 0; i < 32; i++) {
                        arr.push({ id: "acc" + i, displayName: "Account " + i });
                    }
                    return arr;
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("32-accounts"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 32);
        for (i, output) in result.outputs.iter().enumerate() {
            assert_eq!(output.account.id, format!("acc{}", i));
        }
    }
    #[test]
    fn discovery_account_context_handles_special_characters() {
        // Account ids/displayNames with quotes and newlines must be handled safely.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        lines: [ctx.line.text({ label: "ID", value: ctx.account.id })]
                    };
                },
                discoverAccounts(ctx) {
                    return [
                        { id: "it's \"quoted\"", displayName: "Name with 'quotes' and \n newline" }
                    ];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("special-chars"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account.id, "it's \"quoted\"");
        assert_eq!(
            result.outputs[0].account.display_name,
            "Name with 'quotes' and \n newline"
        );
    }

    #[test]
    fn legacy_probe_without_discover_accounts_has_default_account() {
        // Legacy plugin without discoverAccounts gets default account context.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        lines: [ctx.line.text({ label: "Account", value: ctx.account.id })]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("legacy-default"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn discover_accounts_getter_exception_produces_error() {
        // A Proxy that throws on get("discoverAccounts") must produce a
        // provider error, not fall back to legacy mode.
        let plugin = test_plugin(
            r#"
            var handler = {
                get: function(target, prop) {
                    if (prop === "discoverAccounts") {
                        throw "getter-boom";
                    }
                    return target[prop];
                }
            };
            var proxy = new Proxy({
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                }
            }, handler);
            globalThis.__openusage_plugin = proxy;
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("getter-exception"), "0.0.0", None);
        assert_eq!(
            result.outputs.len(),
            1,
            "getter exception should produce exactly one error output"
        );
        assert_eq!(
            result.outputs[0].account,
            AccountRef::default_account(),
            "getter exception must produce error output with default account"
        );
        let err = error_text(&result.outputs[0]);
        assert!(
            err.contains("accessor threw"),
            "error text should mention accessor threw: {}",
            err
        );
    }

    #[test]
    fn account_context_property_is_immutable() {
        // The `account` property on the probe context must be read-only.
        // Attempting to reassign ctx.account must silently fail or throw.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    var before = ctx.account.id;
                    try { ctx.account = { id: "hacked", displayName: "Hacked" }; } catch(e) {}
                    var after = ctx.account.id;
                    return {
                        lines: [
                            ctx.line.text({ label: "Before", value: before }),
                            ctx.line.text({ label: "After", value: after })
                        ]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("immutable-prop"), "0.0.0", None);
        let lines = &result.outputs[0].lines;
        let get_text = |label: &str| -> String {
            lines
                .iter()
                .find_map(|l| match l {
                    MetricLine::Text {
                        label: l, value, ..
                    } if l == label => Some(value.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        assert_eq!(get_text("Before"), "default");
        assert_eq!(
            get_text("After"),
            "default",
            "ctx.account must remain 'default' after reassignment attempt"
        );
    }

    #[test]
    fn account_context_account_id_and_display_name_are_immutable() {
        // The account object's id and displayName must be read-only via
        // Property API (non-writable, non-configurable).
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    var before = ctx.account.id;
                    try { ctx.account.id = "hacked-id"; } catch(e) {}
                    try { ctx.account.displayName = "Hacked Name"; } catch(e) {}
                    var after = ctx.account.id;
                    var desc = Object.getOwnPropertyDescriptor(ctx.account, "id");
                    return {
                        lines: [
                            ctx.line.text({ label: "Before", value: before }),
                            ctx.line.text({ label: "After", value: after }),
                            ctx.line.text({ label: "Writable", value: String(desc.writable) }),
                            ctx.line.text({ label: "Configurable", value: String(desc.configurable) })
                        ]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("frozen-account"), "0.0.0", None);
        let lines = &result.outputs[0].lines;
        let get_text = |label: &str| -> String {
            lines
                .iter()
                .find_map(|l| match l {
                    MetricLine::Text {
                        label: l, value, ..
                    } if l == label => Some(value.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        assert_eq!(get_text("Before"), "default");
        assert_eq!(
            get_text("After"),
            "default",
            "account.id must remain 'default' after mutation attempt"
        );
        assert_eq!(
            get_text("Writable"),
            "false",
            "account.id must be non-writable"
        );
        assert_eq!(
            get_text("Configurable"),
            "false",
            "account.id must be non-configurable"
        );
    }

    #[test]
    fn discovery_account_context_has_immutable_account() {
        // In discovery mode, each account context's account property must
        // also be immutable.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    var before = ctx.account.id;
                    try { ctx.account = { id: "hacked", displayName: "Hacked" }; } catch(e) {}
                    var after = ctx.account.id;
                    var desc = Object.getOwnPropertyDescriptor(ctx, "account");
                    return {
                        lines: [
                            ctx.line.text({ label: "Before", value: before }),
                            ctx.line.text({ label: "After", value: after }),
                            ctx.line.text({ label: "Writable", value: String(desc.writable) }),
                            ctx.line.text({ label: "Configurable", value: String(desc.configurable) })
                        ]
                    };
                },
                discoverAccounts(ctx) {
                    return [{ id: "work", displayName: "Work Account" }];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("disc-immutable"), "0.0.0", None);
        let lines = &result.outputs[0].lines;
        let get_text = |label: &str| -> String {
            lines
                .iter()
                .find_map(|l| match l {
                    MetricLine::Text {
                        label: l, value, ..
                    } if l == label => Some(value.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        assert_eq!(get_text("Before"), "work");
        assert_eq!(
            get_text("After"),
            "work",
            "ctx.account.id must remain 'work' after reassignment attempt"
        );
        assert_eq!(
            get_text("Writable"),
            "false",
            "ctx.account property must be non-writable"
        );
        assert_eq!(
            get_text("Configurable"),
            "false",
            "ctx.account property must be non-configurable"
        );
    }

    #[test]
    fn account_context_immutable_when_object_freeze_replaced() {
        // Plugin replaces Object.freeze with a no-op; account identity
        // must still be immutable because Property API is used, not
        // Object.freeze.
        let plugin = test_plugin(
            r#"
            // Replace Object.freeze with a no-op to simulate monkey-patching.
            Object.freeze = function(obj) { return obj; };

            globalThis.__openusage_plugin = {
                probe(ctx) {
                    var before = ctx.account.id;
                    try { ctx.account.id = "hacked-id"; } catch(e) {}
                    var after = ctx.account.id;
                    var desc = Object.getOwnPropertyDescriptor(ctx.account, "id");
                    return {
                        lines: [
                            ctx.line.text({ label: "Before", value: before }),
                            ctx.line.text({ label: "After", value: after }),
                            ctx.line.text({ label: "Writable", value: String(desc.writable) })
                        ]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("freeze-replaced"), "0.0.0", None);
        let lines = &result.outputs[0].lines;
        let get_text = |label: &str| -> String {
            lines
                .iter()
                .find_map(|l| match l {
                    MetricLine::Text {
                        label: l, value, ..
                    } if l == label => Some(value.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        assert_eq!(get_text("Before"), "default");
        assert_eq!(
            get_text("After"),
            "default",
            "account.id must remain 'default' even when Object.freeze is replaced"
        );
        assert_eq!(
            get_text("Writable"),
            "false",
            "account.id must be non-writable via Property API"
        );
    }

    #[test]
    fn discovery_explicit_default_account_uses_discovered_identity() {
        // In discovery mode, an explicit {id:"default",displayName:"default"}
        // is equivalent to unspecified and must receive the discovered identity.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { id: "default", displayName: "default" },
                        lines: [ctx.line.text({ label: "Account", value: ctx.account.id })]
                    };
                },
                discoverAccounts(ctx) {
                    return [{ id: "work", displayName: "Work Account" }];
                }
            };
            "#,
        );
        let result = run_probe(
            &plugin,
            &temp_app_dir("disc-explicit-default"),
            "0.0.0",
            None,
        );
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(
            result.outputs[0].account.id, "work",
            "explicit default must be replaced by discovered identity"
        );
        assert_eq!(result.outputs[0].account.display_name, "Work Account");
    }

    #[test]
    fn discovery_mismatched_id_is_account_specific_error() {
        // A returned account with a mismatched id must produce an
        // account-specific error carrying the discovered identity.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { id: "wrong-id", displayName: "Work Account" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                },
                discoverAccounts(ctx) {
                    return [{ id: "work", displayName: "Work Account" }];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("mismatch-id"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        // Error must carry the discovered account, not default.
        assert_eq!(
            result.outputs[0].account.id, "work",
            "mismatched id error must carry discovered account identity"
        );
        assert!(
            error_text(&result.outputs[0]).contains("does not match discovered identity"),
            "error must use generic mismatch message"
        );
    }

    #[test]
    fn discovery_mismatched_display_name_is_account_specific_error() {
        // A returned account with a mismatched displayName must produce an
        // account-specific error carrying the discovered identity.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { id: "work", displayName: "Wrong Name" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                },
                discoverAccounts(ctx) {
                    return [{ id: "work", displayName: "Work Account" }];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("mismatch-dn"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        // Error must carry the discovered account, not default.
        assert_eq!(
            result.outputs[0].account.id, "work",
            "mismatched displayName error must carry discovered account identity"
        );
        assert!(
            error_text(&result.outputs[0]).contains("does not match discovered identity"),
            "error must use generic mismatch message"
        );
    }

    #[test]
    fn discovery_null_uses_legacy_mode() {
        // discoverAccounts: null must use legacy mode with default account.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts: null
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("null-disc"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn discovery_undefined_uses_legacy_mode() {
        // Explicit discoverAccounts: undefined must use legacy mode.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts: undefined
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("undefined-disc"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn discovery_rejected_async_produces_provider_error() {
        // An async discoverAccounts that rejects must produce one
        // default-account provider error output.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                async discoverAccounts(ctx) {
                    throw "async-discovery-boom";
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("async-reject"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        assert!(error_text(&result.outputs[0]).contains("async-discovery-boom"));
    }

    #[test]
    fn discovery_async_sequential_probes_preserve_order_and_state() {
        // Sequential async account probes must preserve discovery order
        // and correctly read account-keyed closure state set up by
        // discoverAccounts.
        let plugin = test_plugin(
            r#"
            var accountState = {};
            globalThis.__openusage_plugin = {
                async probe(ctx) {
                    var st = accountState[ctx.account.id];
                    if (!st) {
                        throw "no state for " + ctx.account.id;
                    }
                    var consumed = st.value;
                    delete accountState[ctx.account.id];
                    return {
                        lines: [
                            ctx.line.text({ label: "Account", value: ctx.account.id }),
                            ctx.line.text({ label: "Data", value: consumed })
                        ]
                    };
                },
                async discoverAccounts(ctx) {
                    accountState["first"] = { value: "state-for-first" };
                    accountState["second"] = { value: "state-for-second" };
                    return [
                        { id: "first", displayName: "First" },
                        { id: "second", displayName: "Second" }
                    ];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("async-seq"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 2);
        // Order must be preserved.
        assert_eq!(result.outputs[0].account.id, "first");
        assert_eq!(result.outputs[1].account.id, "second");
        // Each probe consumed the correct account-keyed state.
        let get_data = |output: &PluginOutput| -> String {
            output
                .lines
                .iter()
                .find_map(|l| match l {
                    MetricLine::Text {
                        label: l, value, ..
                    } if l == "Data" => Some(value.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        };
        assert_eq!(get_data(&result.outputs[0]), "state-for-first");
        assert_eq!(get_data(&result.outputs[1]), "state-for-second");
    }

    #[test]
    fn discovery_throwing_account_getter_isolates_that_account() {
        // A probe that returns a result with a throwing account getter
        // must produce an error for that account only, while the next
        // account still succeeds.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    var result = {
                        lines: [ctx.line.text({ label: "Account", value: ctx.account.id })]
                    };
                    if (ctx.account.id === "broken") {
                        Object.defineProperty(result, "account", {
                            get: function() { throw "getter-boom"; },
                            enumerable: true,
                            configurable: true
                        });
                    }
                    return result;
                },
                discoverAccounts(ctx) {
                    return [
                        { id: "broken", displayName: "Broken" },
                        { id: "good", displayName: "Good" }
                    ];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("getter-isolate"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 2);
        // First (broken) account: error carrying discovered identity.
        assert_eq!(result.outputs[0].account.id, "broken");
        assert!(
            result.outputs[0].lines[0]
                .to_string()
                .contains("accessor threw"),
            "broken account should have error about accessor"
        );
        // Second (good) account: succeeds.
        assert_eq!(result.outputs[1].account.id, "good");
        assert!(
            result.outputs[1].lines[0].to_string().contains("good"),
            "good account should succeed"
        );
    }
}
