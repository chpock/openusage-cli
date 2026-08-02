/// Shared test support for the override lifecycle architecture.
///
/// Provides two explicit APIs backed by the production
/// `override_lifecycle::run_lifecycle` function and `execute_provider_in_context`:
///
/// - `run_lifecycle_contract` — lifecycle-contract runner for `tests/override_runner.rs`.
///   Uses `run_lifecycle` directly with `before_plugin`/`after_lifecycle` hooks.
///   Returns `LifecycleOutcome` (Json, Loaded, or LifecycleError).
///
/// - `run_provider_probe` — provider-probe runner for Codex/Copilot integration tests.
///   Calls `execute_provider_in_context` (production probe path).
///   Returns `ProviderOutcome::Probe { result, assertion }`.
///
/// Both runners share the same context setup (`setup_quickjs_context`) and
/// the same JS evaluation/result helper (`eval_and_decode_script`),
/// eliminating duplicated harness/setup/Promise/JSON/decode logic.
use openusage_cli::plugin_engine::manifest::{LoadedPlugin, PluginManifest};
use openusage_cli::plugin_engine::override_lifecycle::{self, JsError, LifecycleStage};
use openusage_cli::plugin_engine::runtime::{self as runtime_mod, FileSubscription, ProbeResult};
use rquickjs::{Context, Ctx, Runtime};
use serde_json::Value;
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════════════
// Shared context setup
// ═══════════════════════════════════════════════════════════════════════

/// Create a QuickJS Runtime and full Context. Returns `Err(JsError)` on failure.
fn setup_quickjs_context() -> Result<(Runtime, Context), JsError> {
    let rt = Runtime::new()
        .map_err(|e| JsError::new(LifecycleStage::Runtime, format!("quickjs runtime: {}", e)))?;
    let ctx = Context::full(&rt)
        .map_err(|e| JsError::new(LifecycleStage::Context, format!("quickjs context: {}", e)))?;
    Ok((rt, ctx))
}

// ═══════════════════════════════════════════════════════════════════════
// Shared JS evaluation / result helper
// ═══════════════════════════════════════════════════════════════════════

/// Result of evaluating a JS script: either a JSON-parsed Value, or a
/// typed `JsError` with the exact stage (Execution or ResultDecode).
#[derive(Debug)]
pub enum ScriptResult {
    /// Script returned a valid JSON string.
    Json(Value),
    /// Script execution failed (JS throw/rejection) — carries JsError
    /// with LifecycleStage::Execution and full Error-object details.
    ExecutionError(JsError),
    /// Script returned a non-string value, or a string that is not
    /// valid JSON — carries JsError with LifecycleStage::ResultDecode.
    DecodeError(JsError),
}

/// Evaluate a JS script in the given context and classify the result.
///
/// - JS throw/rejection => `ExecutionError` with shared JsError details.
/// - Successful non-string value => `DecodeError` with value-type diagnostic.
/// - Successful string that is not valid JSON => `DecodeError` with parse error.
/// - Successful string containing valid JSON => `Json(parsed)`.
pub fn eval_and_decode_script(ctx: &Ctx<'_>, script: &str) -> ScriptResult {
    if script.is_empty() {
        return ScriptResult::ExecutionError(JsError::new(
            LifecycleStage::Execution,
            "execution script is empty",
        ));
    }

    match ctx.eval::<rquickjs::Value, _>(script.as_bytes()) {
        Ok(val) => {
            // Resolve Promise if needed (via the shared canonical helper)
            let resolved = match override_lifecycle::resolve_sync_or_promise(
                ctx,
                val,
                LifecycleStage::Execution,
            ) {
                Ok(v) => v,
                Err(js_err) => return ScriptResult::ExecutionError(js_err),
            };

            // Classify the result value
            if let Some(s) = resolved.as_string()
                && let Ok(text) = s.to_string()
            {
                // Returned a string — try JSON decode
                match serde_json::from_str(&text) {
                    Ok(json_val) => ScriptResult::Json(json_val),
                    Err(e) => ScriptResult::DecodeError(JsError::new(
                        LifecycleStage::ResultDecode,
                        format!(
                            "result is not valid JSON: {} (raw: {})",
                            e,
                            &text[..text.len().min(80)]
                        ),
                    )),
                }
            } else {
                // Non-string value — type diagnostic
                let type_name = if resolved.is_number() {
                    "number"
                } else if resolved.is_bool() {
                    "boolean"
                } else if resolved.is_object() {
                    "object"
                } else if resolved.is_null() {
                    "null"
                } else if resolved.is_undefined() {
                    "undefined"
                } else {
                    "unknown"
                };
                ScriptResult::DecodeError(JsError::new(
                    LifecycleStage::ResultDecode,
                    format!(
                        "execution returned non-string value of type '{}'",
                        type_name
                    ),
                ))
            }
        }
        Err(_) => {
            ScriptResult::ExecutionError(JsError::from_caught(LifecycleStage::Execution, ctx))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Shared before-hook factory
// ═══════════════════════════════════════════════════════════════════════

/// Build a `before_plugin` hook that evaluates harness then setup JS.
/// Each stage reports its own exact failure stage (Harness or Setup).
pub fn make_before_hook<'a>(
    harness: &'a str,
    setup: &'a str,
) -> impl Fn(&Ctx<'_>) -> Result<(), JsError> + 'a {
    move |ctx: &Ctx<'_>| -> Result<(), JsError> {
        if !harness.is_empty() {
            ctx.eval::<(), _>(harness.as_bytes())
                .map_err(|_| JsError::from_caught(LifecycleStage::Harness, ctx))?;
        }
        if !setup.is_empty() {
            ctx.eval::<(), _>(setup.as_bytes())
                .map_err(|_| JsError::from_caught(LifecycleStage::Setup, ctx))?;
        }
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Lifecycle-contract runner (for tests/override_runner.rs)
// ═══════════════════════════════════════════════════════════════════════

/// Description of a lifecycle contract test.
#[allow(dead_code)]
pub struct LifecycleSpec<'a> {
    pub plugin_id: &'a str,
    pub plugin_source: &'a str,
    pub override_source: Option<&'a str>,
    pub harness: &'a str,
    pub setup: &'a str,
    pub execution: Option<&'a str>,
}

/// Outcome of running a `LifecycleSpec`.
#[derive(Debug)]
#[allow(dead_code)]
pub enum LifecycleOutcome {
    Json(Value),
    Loaded {
        script: String,
        patched_functions: Vec<String>,
    },
    LifecycleError {
        stage: JsError,
        plugin_id: String,
    },
}

/// Run a lifecycle contract test.
///
/// Ordering:
/// 1. before_plugin hook (harness → setup)
/// 2. AST transform
/// 3. Plugin eval
/// 4. Override bootstrap + override eval (if override source present)
/// 5. after_lifecycle hook (execution script, if present)
///
/// In load-only mode (execution = None), step 5 is skipped.
#[allow(dead_code)]
pub fn run_lifecycle_contract(spec: &LifecycleSpec<'_>) -> LifecycleOutcome {
    let (_, ctx) = match setup_quickjs_context() {
        Ok(pair) => pair,
        Err(stage) => {
            return LifecycleOutcome::LifecycleError {
                stage,
                plugin_id: spec.plugin_id.to_string(),
            };
        }
    };

    ctx.with(|ctx| {
        let before_fn = make_before_hook(spec.harness, spec.setup);

        let exec_result: Cell<Option<ScriptResult>> = Cell::new(None);
        let after_fn = |ctx: &Ctx<'_>| -> Result<(), JsError> {
            let Some(exec_js) = spec.execution else {
                return Ok(());
            };
            let result = eval_and_decode_script(ctx, exec_js);
            match result {
                ScriptResult::ExecutionError(err) => {
                    let msg = err.message.clone();
                    exec_result.set(Some(ScriptResult::ExecutionError(JsError::new(
                        LifecycleStage::Execution,
                        msg.clone(),
                    ))));
                    // Return the original JsError (preserves error_name, stack)
                    Err(err)
                }
                _ => {
                    exec_result.set(Some(result));
                    Ok(())
                }
            }
        };

        let lifecycle_result = match override_lifecycle::run_lifecycle(
            &ctx,
            spec.plugin_id,
            spec.plugin_source,
            spec.override_source,
            Some(&before_fn),
            Some(&after_fn),
        ) {
            Ok(r) => r,
            Err(err) => {
                let js_err = match &err {
                    override_lifecycle::OverrideLifecycleError::Transform(e) => e.clone(),
                    override_lifecycle::OverrideLifecycleError::PluginEval(e) => e.clone(),
                    override_lifecycle::OverrideLifecycleError::OverrideApiInject(e) => e.clone(),
                    override_lifecycle::OverrideLifecycleError::OverrideEval(e) => e.clone(),
                    override_lifecycle::OverrideLifecycleError::OverrideManifest(e) => e.clone(),
                    override_lifecycle::OverrideLifecycleError::Hook(e) => e.clone(),
                };
                return LifecycleOutcome::LifecycleError {
                    stage: js_err,
                    plugin_id: spec.plugin_id.to_string(),
                };
            }
        };

        if spec.execution.is_some() {
            match exec_result.into_inner() {
                Some(ScriptResult::Json(val)) => LifecycleOutcome::Json(val),
                Some(ScriptResult::ExecutionError(err)) => LifecycleOutcome::LifecycleError {
                    stage: err,
                    plugin_id: spec.plugin_id.to_string(),
                },
                Some(ScriptResult::DecodeError(err)) => LifecycleOutcome::LifecycleError {
                    stage: err,
                    plugin_id: spec.plugin_id.to_string(),
                },
                None => LifecycleOutcome::LifecycleError {
                    stage: JsError::new(
                        LifecycleStage::Execution,
                        "execution did not produce a result".to_string(),
                    ),
                    plugin_id: spec.plugin_id.to_string(),
                },
            }
        } else {
            LifecycleOutcome::Loaded {
                script: lifecycle_result.script,
                patched_functions: lifecycle_result.patched_functions,
            }
        }
    })
}

// ═══════════════════════════════════════════════════════════════════════
// Provider-probe runner (for codex_override, copilot_override)
// ═══════════════════════════════════════════════════════════════════════

/// Description of a provider probe test.
pub struct ProviderSpec<'a> {
    pub plugin_id: &'a str,
    pub plugin_name: &'a str,
    pub plugin_source: &'a str,
    pub override_source: Option<&'a str>,
    pub harness: &'a str,
    pub setup: &'a str,
    pub assertion: Option<&'a str>,
}

/// Outcome of running a `ProviderSpec`.
#[derive(Debug)]
pub enum ProviderOutcome {
    Probe {
        result: ProbeResult,
        assertion: Result<Value, JsError>,
    },
}

/// Run a provider probe test through `execute_provider_in_context`.
///
/// Steps:
/// 1. before_plugin hook (harness → setup) passed to `run_lifecycle`
/// 2. Creates a minimal `LoadedPlugin` and calls `execute_provider_in_context`
/// 3. Optional post-probe assertion JS evaluated after probing
pub fn run_provider_probe(spec: &ProviderSpec<'_>) -> ProviderOutcome {
    let display_name = if spec.plugin_name.is_empty() {
        spec.plugin_id
    } else {
        spec.plugin_name
    };

    let (_, ctx) = match setup_quickjs_context() {
        Ok(pair) => pair,
        Err(_) => {
            return ProviderOutcome::Probe {
                result: ProbeResult {
                    outputs: Vec::new(),
                    subscriptions: Vec::new(),
                },
                assertion: Err(JsError::new(
                    LifecycleStage::Runtime,
                    "quickjs context setup failed".to_string(),
                )),
            };
        }
    };

    ctx.with(|ctx| {
        let before_fn = make_before_hook(spec.harness, spec.setup);

        // Create a minimal LoadedPlugin fixture
        let plugin = LoadedPlugin {
            manifest: PluginManifest {
                schema_version: 1,
                id: spec.plugin_id.to_string(),
                name: display_name.to_string(),
                version: "0.0.0".to_string(),
                entry: "plugin.js".to_string(),
                icon: "icon.svg".to_string(),
                brand_color: None,
                lines: vec![],
                links: vec![],
            },
            plugin_dir: PathBuf::from("."),
            entry_script: spec.plugin_source.to_string(),
            icon_data_url: "data:image/svg+xml;base64,".to_string(),
        };

        let subs: Arc<std::sync::Mutex<Vec<FileSubscription>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        // Call execute_provider_in_context with hooks
        let result = runtime_mod::execute_provider_in_context(
            &ctx,
            &plugin,
            spec.override_source,
            &subs,
            Some(&before_fn),
            None,
        );

        // Post-probe assertion — use the shared eval/decode helper
        let assertion: Result<Value, JsError> = match spec.assertion {
            Some(js) => match eval_and_decode_script(&ctx, js) {
                ScriptResult::Json(val) => Ok(val),
                ScriptResult::ExecutionError(err) => Err(err),
                ScriptResult::DecodeError(err) => Err(err),
            },
            None => Err(JsError::new(
                LifecycleStage::Execution,
                "no assertion script provided".to_string(),
            )),
        };

        ProviderOutcome::Probe { result, assertion }
    })
}
