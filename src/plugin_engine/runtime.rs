use crate::plugin_engine::host_api;
use crate::plugin_engine::manifest::LoadedPlugin;
use crate::plugin_engine::override_lifecycle;
use rquickjs::object::Property;
use rquickjs::{Array, Context, Ctx, Error, Object, Runtime, Value};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
    pub origin: String,
    #[serde(default = "account_active_default")]
    pub is_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

fn account_active_default() -> bool {
    true
}

impl AccountRef {
    /// Default account used when no explicit account is provided.
    pub fn default_account() -> Self {
        Self {
            id: "default".to_string(),
            origin: "native".to_string(),
            is_active: true,
            name: None,
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
/// In single-account mode (no `discoverAccounts`), `outputs` contains one item.
/// In discovery mode, `outputs` contains one item per discovered account
/// (or zero for an empty valid discovery list, or one error output on
/// discovery failure).
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub outputs: Vec<PluginOutput>,
    pub subscriptions: Vec<FileSubscription>,
}

/// Execute a provider probe within a sandboxed environment.
///
/// Creates a QuickJS runtime, injects host API with sandboxed filesystem
/// access (rooted at `sandbox_root`), runs the override lifecycle, and
/// performs discovery/probe as normal.
///
/// This is a convenience wrapper for integration tests that need real
/// filesystem access within a restricted sandbox.
pub fn run_probe_in_sandbox(
    plugin: &LoadedPlugin,
    app_data_dir: &Path,
    app_version: &str,
    plugin_overrides_dir: Option<&Path>,
    sandbox_root: &Path,
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

    let override_source = override_script
        .as_ref()
        .map(|loaded| loaded.script.as_str());

    ctx.with(|ctx| {
        let subs_pass = Arc::clone(&subs);
        if host_api::inject_host_api_in_sandbox(
            &ctx,
            &plugin_id,
            app_data_dir,
            app_version,
            subs_pass,
            sandbox_root,
        )
        .is_err()
        {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    "host api injection failed".to_string(),
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

        execute_provider_in_context(&ctx, plugin, override_source, &subs, None, None)
    })
}
///
/// This is the production probe path used by the test harness. It:
/// 1. Runs the override lifecycle (AST transform + plugin eval + override bootstrap/eval)
/// 2. Looks up `__openusage_plugin` and `probe`
/// 3. Checks for `discoverAccounts` capability
/// 4. Runs single-account or discovery-mode probe
/// 5. Returns the result with collected subscriptions
///
/// The caller is responsible for host API injection and context setup.
/// Optional `before_plugin` and `after_lifecycle` hooks are forwarded to
/// `override_lifecycle::run_lifecycle`.
#[allow(clippy::type_complexity)]
pub fn execute_provider_in_context<'js>(
    ctx: &Ctx<'js>,
    plugin: &LoadedPlugin,
    override_script: Option<&str>,
    subs: &Arc<std::sync::Mutex<Vec<FileSubscription>>>,
    before_plugin: Option<&dyn Fn(&Ctx<'_>) -> Result<(), override_lifecycle::JsError>>,
    after_lifecycle: Option<&dyn Fn(&Ctx<'_>) -> Result<(), override_lifecycle::JsError>>,
) -> ProbeResult {
    let plugin_id = &plugin.manifest.id;
    let display_name = &plugin.manifest.name;
    let icon_url = &plugin.icon_data_url;

    // Run the override lifecycle (AST transform + plugin eval + override bootstrap/eval).
    let lifecycle_script = match override_lifecycle::run_lifecycle(
        ctx,
        plugin_id,
        &plugin.entry_script,
        override_script,
        before_plugin,
        after_lifecycle,
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
            let user_msg = match &err {
                override_lifecycle::OverrideLifecycleError::Transform(e) => {
                    format!("script transform failed: {}", e)
                }
                override_lifecycle::OverrideLifecycleError::PluginEval(e) => {
                    format!("script eval failed: {}", e)
                }
                override_lifecycle::OverrideLifecycleError::OverrideApiInject(e) => {
                    format!("plugin override failed: {}", e)
                }
                override_lifecycle::OverrideLifecycleError::OverrideEval(e) => {
                    format!("plugin override failed: {}", e)
                }
                override_lifecycle::OverrideLifecycleError::OverrideManifest(e) => {
                    format!("plugin override failed: {}", e)
                }
                override_lifecycle::OverrideLifecycleError::Hook(e) => {
                    format!("lifecycle hook error: {}", e)
                }
            };
            return ProbeResult {
                outputs: vec![error_output(plugin, user_msg)],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
    };
    // Keep `lifecycle_script` alive for the duration of the function
    // so the allocated String isn't dropped before we're done with ctx.
    let _ = lifecycle_script;

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

    if discover_val.is_function() {
        let discover_fn: rquickjs::Function = discover_val.into_function().unwrap();
        run_discovery_mode(
            ctx,
            plugin,
            &probe_fn,
            &discover_fn,
            &base_ctx,
            subs,
            plugin_id,
            display_name,
            icon_url,
        )
    } else if discover_val.is_null() || discover_val.is_undefined() {
        run_single_account_probe(
            ctx,
            plugin,
            &probe_fn,
            &base_ctx,
            subs,
            plugin_id,
            display_name,
            icon_url,
        )
    } else {
        ProbeResult {
            outputs: vec![error_output(
                plugin,
                "discoverAccounts must be a function or absent".to_string(),
            )],
            subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
        }
    }
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
    let app_data = app_data_dir.to_path_buf();
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

    let override_source = override_script
        .as_ref()
        .map(|loaded| loaded.script.as_str());

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

        execute_provider_in_context(&ctx, plugin, override_source, &subs, None, None)
    })
}

#[allow(clippy::too_many_arguments)]
/// Single-account mode: call probe once with a child context and default account.
fn run_single_account_probe<'js>(
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

    // Parse and validate account descriptors (DiscoveryDescriptor with private errorPolicy).
    let descriptors = match parse_discovery_accounts(&accounts_array) {
        Ok(accs) => accs,
        Err(msg) => {
            return ProbeResult {
                outputs: vec![error_output(plugin, msg)],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
    };

    // Empty valid list: produce zero outputs.
    if descriptors.is_empty() {
        return ProbeResult {
            outputs: Vec::new(),
            subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
        };
    }

    // Precompute probe-id multiplicity to preserve compatibility when ids are unique,
    // and to deterministically canonicalize collisions.
    let mut probe_id_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for descriptor in &descriptors {
        *probe_id_counts
            .entry(descriptor.probe_id.as_str())
            .or_insert(0) += 1;
    }

    // Ensure selected output IDs are unique before probing.
    let mut seen_output_ids = std::collections::HashSet::new();
    for descriptor in &descriptors {
        let duplicate_probe_id_exists = probe_id_counts
            .get(descriptor.probe_id.as_str())
            .copied()
            .unwrap_or(0)
            > 1;
        let output_id = if should_use_canonical_id(duplicate_probe_id_exists) {
            canonical_discovery_account_id(plugin_id, descriptor)
        } else {
            descriptor.probe_id.clone()
        };
        if !seen_output_ids.insert(output_id.clone()) {
            return ProbeResult {
                outputs: vec![error_output(
                    plugin,
                    format!(
                        "discoverAccounts produced duplicate output account id '{}'",
                        output_id
                    ),
                )],
                subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
            };
        }
    }

    // Probe each discovered account sequentially.
    let mut outputs: Vec<(usize, PluginOutput)> = Vec::with_capacity(descriptors.len());
    for (desc_idx, descriptor) in descriptors.iter().enumerate() {
        let duplicate_probe_id_exists = probe_id_counts
            .get(descriptor.probe_id.as_str())
            .copied()
            .unwrap_or(0)
            > 1;
        let output_id = if should_use_canonical_id(duplicate_probe_id_exists) {
            canonical_discovery_account_id(plugin_id, descriptor)
        } else {
            descriptor.probe_id.clone()
        };
        let output_account = AccountRef {
            id: output_id,
            origin: descriptor.origin.clone(),
            is_active: descriptor.is_active,
            name: descriptor.name.clone(),
        };
        let probe_account = AccountRef {
            id: descriptor.probe_id.clone(),
            origin: descriptor.origin.clone(),
            is_active: descriptor.is_active,
            name: None,
        };
        let account_ctx = match create_account_context(ctx, &probe_account, base_ctx) {
            Ok(c) => c,
            Err(_) => {
                outputs.push((
                    desc_idx,
                    error_output_with_account(
                        plugin,
                        "failed to create account context".to_string(),
                        &output_account,
                    ),
                ));
                continue;
            }
        };

        let result_value: Value = match probe_fn.call((account_ctx,)) {
            Ok(r) => r,
            Err(_) => {
                outputs.push((
                    desc_idx,
                    error_output_with_account(plugin, extract_error_string(ctx), &output_account),
                ));
                continue;
            }
        };

        let result = match resolve_js_result(ctx, result_value) {
            Ok(obj) => obj,
            Err(msg) => {
                outputs.push((
                    desc_idx,
                    error_output_with_account(plugin, msg, &output_account),
                ));
                continue;
            }
        };

        // In discovery mode, identity is host-authoritative.
        let output = build_plugin_output(
            &result,
            plugin_id,
            display_name,
            icon_url,
            Some(&output_account),
        );
        outputs.push((desc_idx, output));
    }

    // Apply soft-fail: errorPolicy=hide-if-other-account hides that descriptor's
    // error whenever ANY OTHER descriptor exists, whether that other account
    // succeeds or errors. It applies only to the descriptor carrying the policy.
    // Opencode errors stay visible regardless.
    let has_any_other_descriptor = descriptors.len() > 1;

    let final_outputs: Vec<PluginOutput> = outputs
        .into_iter()
        .filter(|(desc_idx, o)| {
            // If this is an error output and descriptor has hide-if-other-account policy
            let is_error = o
                .lines
                .iter()
                .any(|l| matches!(l, MetricLine::Badge { label, .. } if label == "Error"));
            if is_error
                && desc_idx < &descriptors.len()
                && descriptors[*desc_idx].error_policy == ErrorPolicy::HideIfOtherAccount
                && has_any_other_descriptor
                && o.account.origin != "opencode"
            {
                return false; // hide this error
            }
            true
        })
        .map(|(_, o)| o)
        .collect();

    ProbeResult {
        outputs: final_outputs,
        subscriptions: std::mem::take(&mut *subs.lock().unwrap()),
    }
}

/// Create a child context inheriting from `base_ctx` with an immutable
/// `account` property containing only `{ id }`.
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

/// Private discovery descriptor with error policy (not serialized).
#[derive(Debug, Clone)]
struct DiscoveryDescriptor {
    probe_id: String,
    origin: String,
    is_active: bool,
    name: Option<String>,
    origin_namespace: String,
    stable_subject_key: Option<String>,
    source_ref: Option<String>,
    error_policy: ErrorPolicy,
}

#[derive(Debug, Clone, PartialEq)]
enum ErrorPolicy {
    None,
    HideIfOtherAccount,
}

/// Parse and validate a discovery result array.
/// Returns up to 32 account descriptors with non-empty ids.
/// Each descriptor carries id, origin, and private errorPolicy.
/// No displayName handling.
fn parse_discovery_accounts(array: &Array) -> Result<Vec<DiscoveryDescriptor>, String> {
    let len = array.len();
    if len > 32 {
        return Err(format!(
            "discoverAccounts returned {} accounts (max 32)",
            len
        ));
    }

    let mut accounts = Vec::with_capacity(len);

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

        let probe_id: String = obj
            .get("id")
            .map_err(|_| format!("discoverAccounts: element at index {} missing id", idx))?;
        if probe_id.trim().is_empty() {
            return Err(format!(
                "discoverAccounts: element at index {} has empty id",
                idx
            ));
        }

        let name: Option<String> = match obj.get::<_, rquickjs::Value>("name") {
            Ok(val) => {
                if val.is_null() || val.is_undefined() {
                    None
                } else if let Some(s) = val.as_string() {
                    let raw = s.to_string().unwrap_or_default();
                    if raw.trim().is_empty() {
                        return Err(format!(
                            "discoverAccounts: element at index {} has empty name",
                            idx
                        ));
                    }
                    Some(raw)
                } else {
                    return Err(format!(
                        "discoverAccounts: element at index {} has non-string name",
                        idx
                    ));
                }
            }
            Err(_) => {
                return Err(format!(
                    "discoverAccounts: element at index {} has invalid name (accessor threw)",
                    idx
                ));
            }
        };

        // origin: absent/null => "native"; empty/non-string/accessor => error
        let origin: String = match obj.get::<_, rquickjs::Value>("origin") {
            Ok(val) => {
                if val.is_null() || val.is_undefined() {
                    "native".to_string()
                } else if let Some(s) = val.as_string() {
                    let raw = s.to_string().unwrap_or_default();
                    if raw.trim().is_empty() {
                        return Err(format!(
                            "discoverAccounts: element at index {} has empty origin",
                            idx
                        ));
                    }
                    raw
                } else {
                    return Err(format!(
                        "discoverAccounts: element at index {} has non-string origin",
                        idx
                    ));
                }
            }
            Err(_) => {
                return Err(format!(
                    "discoverAccounts: element at index {} has invalid origin (accessor threw)",
                    idx
                ));
            }
        };

        let is_active: bool = match obj.get::<_, rquickjs::Value>("isActive") {
            Ok(val) => {
                if val.is_null() || val.is_undefined() {
                    true
                } else if let Some(b) = val.as_bool() {
                    b
                } else {
                    return Err(format!(
                        "discoverAccounts: element at index {} has non-boolean isActive",
                        idx
                    ));
                }
            }
            Err(_) => {
                return Err(format!(
                    "discoverAccounts: element at index {} has invalid isActive (accessor threw)",
                    idx
                ));
            }
        };

        let origin_namespace: String = match obj.get::<_, rquickjs::Value>("originNamespace") {
            Ok(val) => {
                if val.is_null() || val.is_undefined() {
                    "default".to_string()
                } else if let Some(s) = val.as_string() {
                    let raw = s.to_string().unwrap_or_default();
                    if raw.trim().is_empty() {
                        return Err(format!(
                            "discoverAccounts: element at index {} has empty originNamespace",
                            idx
                        ));
                    }
                    raw
                } else {
                    return Err(format!(
                        "discoverAccounts: element at index {} has non-string originNamespace",
                        idx
                    ));
                }
            }
            Err(_) => {
                return Err(format!(
                    "discoverAccounts: element at index {} has invalid originNamespace (accessor threw)",
                    idx
                ));
            }
        };

        let stable_subject_key: Option<String> = match obj
            .get::<_, rquickjs::Value>("stableSubjectKey")
        {
            Ok(val) => {
                if val.is_null() || val.is_undefined() {
                    None
                } else if let Some(s) = val.as_string() {
                    let raw = s.to_string().unwrap_or_default();
                    if raw.trim().is_empty() {
                        return Err(format!(
                            "discoverAccounts: element at index {} has empty stableSubjectKey",
                            idx
                        ));
                    }
                    Some(raw)
                } else {
                    return Err(format!(
                        "discoverAccounts: element at index {} has non-string stableSubjectKey",
                        idx
                    ));
                }
            }
            Err(_) => {
                return Err(format!(
                    "discoverAccounts: element at index {} has invalid stableSubjectKey (accessor threw)",
                    idx
                ));
            }
        };

        let source_ref: Option<String> = match obj.get::<_, rquickjs::Value>("sourceRef") {
            Ok(val) => {
                if val.is_null() || val.is_undefined() {
                    None
                } else if let Some(s) = val.as_string() {
                    let raw = s.to_string().unwrap_or_default();
                    if raw.trim().is_empty() {
                        return Err(format!(
                            "discoverAccounts: element at index {} has empty sourceRef",
                            idx
                        ));
                    }
                    Some(raw)
                } else {
                    return Err(format!(
                        "discoverAccounts: element at index {} has non-string sourceRef",
                        idx
                    ));
                }
            }
            Err(_) => {
                return Err(format!(
                    "discoverAccounts: element at index {} has invalid sourceRef (accessor threw)",
                    idx
                ));
            }
        };

        // errorPolicy: private, not serialized
        // absent/null/undefined => None; "hide-if-other-account" => HideIfOtherAccount;
        // any other string, non-string, or accessor error => discovery validation error
        let error_policy: ErrorPolicy = match obj.get::<_, rquickjs::Value>("errorPolicy") {
            Ok(val) => {
                if val.is_null() || val.is_undefined() {
                    ErrorPolicy::None
                } else if let Some(s) = val.as_string() {
                    match s.to_string().unwrap_or_default().as_str() {
                        "hide-if-other-account" => ErrorPolicy::HideIfOtherAccount,
                        _ => {
                            return Err(format!(
                                "discoverAccounts: element at index {} has invalid errorPolicy '{}'",
                                idx,
                                s.to_string().unwrap_or_default()
                            ));
                        }
                    }
                } else {
                    return Err(format!(
                        "discoverAccounts: element at index {} has non-string errorPolicy",
                        idx
                    ));
                }
            }
            Err(_) => {
                return Err(format!(
                    "discoverAccounts: element at index {} has invalid errorPolicy (accessor threw)",
                    idx
                ));
            }
        };

        accounts.push(DiscoveryDescriptor {
            probe_id,
            origin,
            is_active,
            name,
            origin_namespace,
            stable_subject_key,
            source_ref,
            error_policy,
        });
    }

    Ok(accounts)
}

fn canonical_discovery_account_id(provider_id: &str, descriptor: &DiscoveryDescriptor) -> String {
    if descriptor.probe_id == "default" && descriptor.origin == "native" {
        return "default".to_string();
    }

    let subject = descriptor
        .stable_subject_key
        .as_deref()
        .unwrap_or(descriptor.probe_id.as_str());
    let source = descriptor.source_ref.as_deref().unwrap_or("");

    let mut hasher = Sha256::new();
    hasher.update(b"acc_v1|");
    hasher.update(provider_id.as_bytes());
    hasher.update(b"|");
    hasher.update(descriptor.origin.as_bytes());
    hasher.update(b"|");
    hasher.update(descriptor.origin_namespace.as_bytes());
    hasher.update(b"|");
    hasher.update(subject.as_bytes());
    hasher.update(b"|");
    hasher.update(source.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{:x}", digest);
    format!("acc_v1_{}", &hex[..24])
}

fn should_use_canonical_id(duplicate_probe_id_exists: bool) -> bool {
    duplicate_probe_id_exists
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
/// The account is host-authoritative: never read from `result.account`.
/// In single-account mode (`discovery_account` is None), the default account is used.
/// In discovery mode (`discovery_account` is Some), the discovered identity is used.
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

    let account = discovery_account
        .cloned()
        .unwrap_or_else(AccountRef::default_account);

    PluginOutput {
        provider_id: plugin_id.to_string(),
        display_name: display_name.to_string(),
        plan,
        lines,
        icon_url: icon_url.to_string(),
        account,
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
        script: override_script,
    }))
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

#[allow(dead_code)]
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
    // Try as an Error object (throw new Error(...))
    if let Some(obj) = exc.as_object() {
        if let Ok(msg) = obj.get::<_, String>("message") {
            let trimmed = msg.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
        if let Ok(msg) = obj.get::<_, String>("name") {
            let trimmed = msg.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
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
        assert_eq!(obj.get("origin").and_then(|v| v.as_str()), Some("native"));
        assert_eq!(obj.get("isActive").and_then(|v| v.as_bool()), Some(true));
        assert!(
            obj.get("displayName").is_none(),
            "should not have displayName key"
        );
        assert!(
            obj.get("display_name").is_none(),
            "should not have snake_case key"
        );
        assert!(obj.get("name").is_none(), "default account has no name");
    }

    #[test]
    fn account_name_serializes_when_present() {
        let account = AccountRef {
            id: "acc_v1_test".to_string(),
            origin: "opencode".to_string(),
            is_active: false,
            name: Some("Work Profile".to_string()),
        };
        let json: JsonValue = serde_json::to_value(&account).expect("serialize");
        let obj = json.as_object().expect("object");
        assert_eq!(obj.get("id").and_then(|v| v.as_str()), Some("acc_v1_test"));
        assert_eq!(obj.get("origin").and_then(|v| v.as_str()), Some("opencode"));
        assert_eq!(obj.get("isActive").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            obj.get("name").and_then(|v| v.as_str()),
            Some("Work Profile")
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
    fn account_ref_deserializes_missing_is_active_as_true() {
        let json = r#"{
            "id": "acc-1",
            "origin": "opencode",
            "name": "Work"
        }"#;
        let account: AccountRef = serde_json::from_str(json).expect("deserialize account");
        assert!(account.is_active, "missing isActive must default to true");
    }

    #[test]
    fn plugin_output_never_reads_probe_account() {
        // probe().account is NEVER read, validated, or accessed.
        // In single-account mode, the output always carries the default account.
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
            AccountRef::default_account(),
            "probe().account must never alter the output account in single-account mode"
        );
    }

    #[test]
    fn probe_result_account_ignored_in_single_account_mode() {
        // In single-account mode, probe().account is NEVER read.
        // The output account is always the default.
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
    fn null_probe_result_account_defaults_to_default() {
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
        // probe().account is never read, output always has default account
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn undefined_probe_result_account_defaults_to_default() {
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
    fn probe_account_non_object_does_not_cause_error() {
        // probe().account is never read, so a non-object value produces
        // normal success output with default account.
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
            "probe().account is never read - output is success with default account"
        );
        assert_eq!(result.outputs[0].lines.len(), 1);
        // Lines should be success, not error
        match &result.outputs[0].lines[0] {
            MetricLine::Text { label, value, .. } => {
                assert_eq!(label, "Status");
                assert_eq!(value, "ok");
            }
            other => panic!("expected text line, got {:?}", other),
        }
    }

    #[test]
    fn probe_account_empty_id_does_not_cause_error() {
        // probe().account is never read, empty id has no effect.
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
        // Should be success, not error
        assert_eq!(result.outputs[0].lines.len(), 1);
        match &result.outputs[0].lines[0] {
            MetricLine::Text { label, value, .. } => {
                assert_eq!(label, "Status");
                assert_eq!(value, "ok");
            }
            other => panic!("expected text line, got {:?}", other),
        }
    }

    #[test]
    fn probe_account_missing_id_does_not_cause_error() {
        // probe().account is never read, missing id has no effect.
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
        assert_eq!(result.outputs[0].lines.len(), 1);
        match &result.outputs[0].lines[0] {
            MetricLine::Text { .. } => {} // success
            other => panic!("expected text line, got {:?}", other),
        }
    }

    #[test]
    fn probe_account_throwing_accessor_does_not_cause_error() {
        // probe().account accessor throw is NEVER caught — it has no effect.
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
            "throwing accessor on probe().account is never caught - output is success with default account"
        );
        // Should be success, not error
        assert_eq!(result.outputs[0].lines.len(), 1);
        match &result.outputs[0].lines[0] {
            MetricLine::Text { .. } => {} // success
            other => panic!("expected text line, got {:?}", other),
        }
    }

    #[test]
    fn probe_account_empty_display_name_does_not_cause_error() {
        // probe().account is never read, empty displayName has no effect.
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
        assert_eq!(result.outputs[0].lines.len(), 1);
        match &result.outputs[0].lines[0] {
            MetricLine::Text { .. } => {} // success
            other => panic!("expected text line, got {:?}", other),
        }
    }

    #[test]
    fn probe_account_missing_display_name_does_not_cause_error() {
        // probe().account is never read, missing displayName has no effect.
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
        assert_eq!(result.outputs[0].lines.len(), 1);
        match &result.outputs[0].lines[0] {
            MetricLine::Text { .. } => {} // success
            other => panic!("expected text line, got {:?}", other),
        }
    }

    #[test]
    fn probe_account_non_object_string_does_not_cause_error() {
        // probe().account as a string is not read, no error.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: "some-string",
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("string-account"), "0.0.0", None);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        assert_eq!(result.outputs[0].lines.len(), 1);
        match &result.outputs[0].lines[0] {
            MetricLine::Text { .. } => {} // success
            other => panic!("expected text line, got {:?}", other),
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
    fn override_replace_discover_accounts_adds_to_single_account_plugin() {
        // A single-account plugin with only probe() gains discoverAccounts via replace.
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

            // originalDiscoverAccounts must be null for a single-account plugin
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
        // When replace added discovery to a single-account plugin, reset removes it.
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
    fn single_account_probe_has_immutable_default_account_context() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    var acct = ctx.account;
                    var id = acct.id;
                    var stillDefault = ctx.account.id === "default";
                    var desc = Object.getOwnPropertyDescriptor(acct, "id");
                    return {
                        lines: [
                            ctx.line.text({ label: "ID", value: id }),
                            ctx.line.text({ label: "StillDefault", value: String(stillDefault) }),
                            ctx.line.text({ label: "Writable", value: String(desc.writable) }),
                            ctx.line.text({ label: "Configurable", value: String(desc.configurable) })
                        ]
                    };
                }
            };
            "#,
        );
        let result = run_probe(
            &plugin,
            &temp_app_dir("single-account-immutable"),
            "0.0.0",
            None,
        );
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
                        lines: [ctx.line.text({ label: "Account", value: ctx.account.id })]
                    };
                },
                discoverAccounts(ctx) {
                    return [
                        { id: "work" },
                        { id: "personal" }
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
        // Host-authoritative: probe().account is never read.
        // The output account always uses the discovered identity.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    if (ctx.account.id === "work") {
                        return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                    }
                    return {
                        account: { id: "mismatched", displayName: "Override" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                },
                discoverAccounts(ctx) {
                    return [
                        { id: "work" },
                        { id: "personal" }
                    ];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("host-auth"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 2);
        // Host-authoritative: accounts come from discovery descriptors, not probe()
        assert_eq!(result.outputs[0].account.id, "work");
        assert_eq!(result.outputs[1].account.id, "personal");
        // Both outputs are success (probe().account is never validated)
        assert!(result.outputs[1].lines[0].to_string().contains("ok"));
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
                    return [{ id: "async-acc" }];
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
                    return [{}];
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
    fn discovery_empty_origin_returns_error() {
        // Empty origin in discovery descriptor must produce an error.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return [{ id: "x", origin: "" }];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("empty-origin"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        assert!(error_text(&result.outputs[0]).contains("empty origin"));
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
                        { id: "dup" },
                        { id: "dup" }
                    ];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("dup-ids"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
        assert!(error_text(&result.outputs[0]).contains("duplicate output account id"));
    }

    #[test]
    fn discovery_duplicate_probe_ids_with_names_have_stable_unique_output_ids() {
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Account", value: ctx.account.id })] };
                },
                discoverAccounts(ctx) {
                    return [
                        { id: "opencode-0", origin: "opencode", originNamespace: "vendor.codex", sourceRef: "vendor:path-a", stableSubjectKey: "u-1", name: "Primary" },
                        { id: "opencode-0", origin: "opencode", originNamespace: "override.codex", sourceRef: "override:path-b", stableSubjectKey: "u-2", name: "Secondary" }
                    ];
                }
            };
            "#,
        );

        let result = run_probe(
            &plugin,
            &temp_app_dir("dup-probe-id-canonical"),
            "0.0.0",
            None,
        );
        assert_eq!(result.outputs.len(), 2);
        assert_ne!(result.outputs[0].account.id, result.outputs[1].account.id);
        for output in &result.outputs {
            assert!(output.account.id.starts_with("acc_v1_"));
            assert_eq!(output.account.origin, "opencode");
            assert!(output.account.name.is_some());
        }

        // Context id remains discover id for plugin internals.
        let line0 = result.outputs[0]
            .lines
            .iter()
            .find_map(|l| match l {
                MetricLine::Text { label, value, .. } if label == "Account" => Some(value),
                _ => None,
            })
            .expect("account line");
        let line1 = result.outputs[1]
            .lines
            .iter()
            .find_map(|l| match l {
                MetricLine::Text { label, value, .. } if label == "Account" => Some(value),
                _ => None,
            })
            .expect("account line");
        assert_eq!(line0, "opencode-0");
        assert_eq!(line1, "opencode-0");
    }

    #[test]
    fn discovery_name_defaults_to_absent_and_rejects_empty() {
        let ok_plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return [{ id: "named", name: "Team A" }];
                }
            };
            "#,
        );
        let ok = run_probe(&ok_plugin, &temp_app_dir("name-present"), "0.0.0", None);
        assert_eq!(ok.outputs.len(), 1);
        assert_eq!(ok.outputs[0].account.id, "named");
        assert_eq!(ok.outputs[0].account.name.as_deref(), Some("Team A"));

        let bad_plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                },
                discoverAccounts(ctx) {
                    return [{ id: "bad", name: "" }];
                }
            };
            "#,
        );
        let bad = run_probe(&bad_plugin, &temp_app_dir("name-empty"), "0.0.0", None);
        assert_eq!(bad.outputs.len(), 1);
        assert!(error_text(&bad.outputs[0]).contains("empty name"));
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
                        arr.push({ id: "a" + i });
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
                        { id: "good" },
                        { id: "broken" },
                        { id: "also-good" }
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
                        { id: "a" },
                        { id: "b" }
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
                return [{ id: "override-acc" }];
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
                    return [{ id: "native" }];
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
                accounts.push({ id: "wrapped" });
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
                        arr.push({ id: "acc" + i });
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
        // Account ids with quotes must be handled safely ctx.account.id.
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
                        { id: "it's \"quoted\"" }
                    ];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("special-chars"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account.id, "it's \"quoted\"");
    }

    #[test]
    fn single_account_probe_without_discover_accounts_has_default_account() {
        // Single-account plugin without discoverAccounts gets default account context.
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
        let result = run_probe(
            &plugin,
            &temp_app_dir("single-account-default"),
            "0.0.0",
            None,
        );
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(result.outputs[0].account, AccountRef::default_account());
    }

    #[test]
    fn discover_accounts_getter_exception_produces_error() {
        // A Proxy that throws on get("discoverAccounts") must produce a
        // provider error, not fall back to single-account mode.
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
    fn account_context_account_id_is_immutable() {
        // The account object's id must be read-only via
        // Property API (non-writable, non-configurable).
        let plugin = test_plugin(
            r#"
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
                    return [{ id: "work" }];
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
    fn probe_result_account_never_read_in_discovery_mode() {
        // probe().account is NEVER read in discovery mode.
        // The output account always uses the discovered identity.
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
                    return [{ id: "work" }];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("disc-never-read"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(
            result.outputs[0].account.id, "work",
            "probe().account is never read - discovered identity is always used"
        );
        // Output is success (not an error)
        assert!(result.outputs[0].lines[0].to_string().contains("work"));
    }

    #[test]
    fn probe_result_account_mismatch_has_no_effect_in_discovery() {
        // probe().account is NEVER read, even with mismatched id.
        // The output is success with the discovered identity.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { id: "wrong-id", displayName: "Wrong" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                },
                discoverAccounts(ctx) {
                    return [{ id: "work" }];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("mismatch-no-effect"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        // Account from discovery, not from probe().account
        assert_eq!(
            result.outputs[0].account.id, "work",
            "probe().account must not alter the output account"
        );
        // Output is success
        assert!(result.outputs[0].lines[0].to_string().contains("ok"));
    }

    #[test]
    fn probe_result_account_with_any_content_has_no_effect() {
        // probe().account content (id, displayName, or anything else)
        // has zero effect on the output account.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    return {
                        account: { id: "work", displayName: "Some Name" },
                        lines: [ctx.line.text({ label: "Status", value: "ok" })]
                    };
                },
                discoverAccounts(ctx) {
                    return [{ id: "work" }];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("any-content"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 1);
        assert_eq!(
            result.outputs[0].account.id, "work",
            "account from descriptor, not from probe().account"
        );
        // Output is success
        assert!(result.outputs[0].lines[0].to_string().contains("ok"));
    }

    #[test]
    fn discovery_null_uses_single_account_mode() {
        // discoverAccounts: null must use single-account mode with default account.
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
    fn discovery_undefined_uses_single_account_mode() {
        // Explicit discoverAccounts: undefined must use single-account mode.
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
    fn probe_account_throwing_getter_has_no_effect() {
        // probe().account accessor is NEVER accessed, so a throwing getter
        // on result.account has no effect.
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
                        { id: "broken" },
                        { id: "good" }
                    ];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("getter-no-effect"), "0.0.0", None);
        assert_eq!(result.outputs.len(), 2);
        // Neither account sees the getter - both succeed
        assert_eq!(result.outputs[0].account.id, "broken");
        assert!(
            result.outputs[0].lines[0].to_string().contains("broken"),
            "broken account should succeed (account getter is never read)"
        );
        assert_eq!(result.outputs[1].account.id, "good");
        assert!(
            result.outputs[1].lines[0].to_string().contains("good"),
            "good account should also succeed"
        );
    }

    #[test]
    fn soft_fail_hides_default_error_only_when_opencode_also_errors() {
        // Regression: errorPolicy=hide-if-other-account on a default descriptor
        // hides that error when ANY OTHER descriptor exists. But opencode errors
        // stay visible. So when both default and opencode error, only the opencode
        // error output is present.
        let plugin = test_plugin(
            r#"
            globalThis.__openusage_plugin = {
                probe(ctx) {
                    if (ctx.account.id === "opencode-0") {
                        throw "opencode error detail";
                    }
                    throw "default error detail";
                },
                discoverAccounts(ctx) {
                    return [
                        { id: "default", origin: "native", errorPolicy: "hide-if-other-account" },
                        { id: "opencode-0", origin: "opencode" }
                    ];
                }
            };
            "#,
        );
        let result = run_probe(&plugin, &temp_app_dir("soft-fail-opencode"), "0.0.0", None);

        // Default error is hidden (hide-if-other-account + other descriptor exists).
        // Opencode error stays visible (opencode errors are never hidden).
        assert_eq!(
            result.outputs.len(),
            1,
            "expected only opencode error output, got {} outputs: {:?}",
            result.outputs.len(),
            result
                .outputs
                .iter()
                .map(|o| &o.account.id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            result.outputs[0].account.id, "opencode-0",
            "remaining output must be the opencode account"
        );
        assert_eq!(
            result.outputs[0].account.origin, "opencode",
            "remaining output must have opencode origin"
        );
        // The opencode error detail should be visible
        assert!(
            error_text(&result.outputs[0]).contains("opencode error detail"),
            "opencode error message must be visible, got: {}",
            error_text(&result.outputs[0])
        );
    }
}
