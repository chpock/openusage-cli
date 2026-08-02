/// Production lifecycle seam for override evaluation.
///
/// Owns the common lifecycle mechanics shared by the production runtime and
/// integration tests: AST transform/preparation, transformed plugin evaluation,
/// and conditional normal override bootstrap/evaluation.
///
/// The lifecycle runner (`run_lifecycle`) accepts two optional Rust hooks:
///
/// - `before_plugin`: called after QuickJS context initialization but BEFORE the
///   plugin script is evaluated. The test runner uses it to evaluate provider-
///   owned harness and setup scripts.
/// - `after_lifecycle`: called AFTER the full lifecycle (transform → plugin eval →
///   override bootstrap → override eval) completes successfully. The test runner
///   uses it to evaluate execution scripts and decode results.
///
/// Production calls `run_lifecycle` with no hooks — identical behavior to the
/// original sealed lifecycle.
///
/// Everything here is `pub(super)`—visible within `plugin_engine` but not
/// exposed publicly. No discovery, probing, or host injection belongs here.
use crate::plugin_engine::{override_api, script_patch};
use rquickjs::{Ctx, Error, Value};

/// Stages that can fail during the override lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleStage {
    /// Harness script evaluation failure (before plugin eval).
    Harness,
    /// Setup script evaluation failure (before plugin eval).
    Setup,
    /// AST transform of the plugin script.
    Transform,
    /// Evaluation of the (possibly transformed) plugin script.
    PluginEval,
    /// Bootstrap of the override API (`__openusage_override`).
    OverrideBootstrap,
    /// Evaluation of the override script.
    OverrideEval,
    /// Validation and installation of the declarative function override manifest.
    OverrideManifest,
    /// Execution script evaluation failure (after lifecycle).
    Execution,
    /// Successful execution that returned non-string or bad JSON.
    ResultDecode,
    /// QuickJS runtime creation failure (runner-level only).
    Runtime,
    /// QuickJS context creation failure (runner-level only).
    Context,
}

impl std::fmt::Display for LifecycleStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LifecycleStage::Harness => write!(f, "harness"),
            LifecycleStage::Setup => write!(f, "setup"),
            LifecycleStage::Transform => write!(f, "transform"),
            LifecycleStage::PluginEval => write!(f, "plugin_eval"),
            LifecycleStage::OverrideBootstrap => write!(f, "override_bootstrap"),
            LifecycleStage::OverrideEval => write!(f, "override_eval"),
            LifecycleStage::OverrideManifest => write!(f, "override_manifest"),
            LifecycleStage::Execution => write!(f, "execution"),
            LifecycleStage::ResultDecode => write!(f, "result_decode"),
            LifecycleStage::Runtime => write!(f, "runtime"),
            LifecycleStage::Context => write!(f, "context"),
        }
    }
}

/// Detailed JS exception information with typed stage.
///
/// Handles both thrown strings (which are common in OpenUsage plugin code) and
/// proper Error objects with name/message/stack trace.
#[derive(Debug, Clone)]
pub struct JsError {
    /// The lifecycle stage that failed.
    pub stage: LifecycleStage,
    /// Human-readable description of the error.
    pub message: String,
    /// If the exception was an Error object, the name property (e.g. "TypeError").
    pub error_name: Option<String>,
    /// If the exception was an Error object, the stack trace.
    pub stack: Option<String>,
}

impl JsError {
    /// Create a new `JsError` with the given stage and raw message text.
    pub fn new(stage: LifecycleStage, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
            error_name: None,
            stack: None,
        }
    }

    /// Create a `JsError` from a QuickJS caught exception.
    pub fn from_caught(stage: LifecycleStage, ctx: &Ctx<'_>) -> Self {
        extract_js_error(ctx, stage)
    }
}

impl std::fmt::Display for JsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ref name) = self.error_name {
            write!(f, "[{}] {}: {}", self.stage, name, self.message)?;
        } else {
            write!(f, "[{}] {}", self.stage, self.message)?;
        }
        Ok(())
    }
}

/// Stage-aware error enum for the override lifecycle.
///
/// Each variant wraps a `JsError` that carries the exact stage, a human-readable
/// message, and (when available) the Error object's name and stack trace.
#[derive(Debug, Clone)]
pub enum OverrideLifecycleError {
    Transform(JsError),
    PluginEval(JsError),
    OverrideApiInject(JsError),
    OverrideEval(JsError),
    /// Error from the declarative function override manifest validation or installation.
    OverrideManifest(JsError),
    /// Error from a lifecycle hook (before_plugin / after_lifecycle).
    /// Preserves the hook's own stage (Harness, Setup, Execution, etc.).
    Hook(JsError),
}

impl std::fmt::Display for OverrideLifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverrideLifecycleError::Transform(err) => write!(f, "{}", err),
            OverrideLifecycleError::PluginEval(err) => write!(f, "{}", err),
            OverrideLifecycleError::OverrideApiInject(err) => write!(f, "{}", err),
            OverrideLifecycleError::OverrideEval(err) => write!(f, "{}", err),
            OverrideLifecycleError::OverrideManifest(err) => write!(f, "{}", err),
            OverrideLifecycleError::Hook(err) => write!(f, "{}", err),
        }
    }
}

/// Extract a detailed `JsError` from the QuickJS context's last exception.
///
/// Inspects the caught value:
/// - If it is a thrown string, that string becomes the message.
/// - If it is an Error object, extracts `name`, `message`, and `stack`.
/// - Falls back to "unknown error" for other exception types.
pub fn extract_js_error(ctx: &Ctx<'_>, stage: LifecycleStage) -> JsError {
    let caught = ctx.catch();
    if caught.is_null() || caught.is_undefined() {
        return JsError {
            stage,
            message: "unknown error".to_string(),
            error_name: None,
            stack: None,
        };
    }

    // Try as a String first (most common in OpenUsage plugin code: `throw "msg"`)
    if let Some(s) = caught.as_string()
        && let Ok(text) = s.to_string()
        && !text.is_empty()
    {
        return JsError {
            stage,
            message: text,
            error_name: None,
            stack: None,
        };
    }

    // Try as an Error object
    if let Some(obj) = caught.as_object() {
        let name: String = obj.get("name").unwrap_or_default();
        let msg: String = obj.get("message").unwrap_or_default();
        let stack: Option<String> = obj.get("stack").ok();
        let display = if msg.is_empty() { name.clone() } else { msg };
        return JsError {
            stage,
            message: display,
            error_name: if name.is_empty() { None } else { Some(name) },
            stack,
        };
    }

    // Fallback: try converting to string (duplicate of above for non-error types)
    if let Some(s) = caught.as_string()
        && let Ok(text) = s.to_string()
        && !text.is_empty()
    {
        return JsError {
            stage,
            message: text,
            error_name: None,
            stack: None,
        };
    }

    JsError {
        stage,
        message: "unknown error".to_string(),
        error_name: None,
        stack: None,
    }
}

/// Resolve a JS value that may be a sync value or a Promise.
///
/// Handles four cases:
/// - **Sync value**: returned as-is.
/// - **Fulfilled Promise**: returns the resolved value.
/// - **Rejected Promise**: extracts `JsError` from the context's caught exception
///   (preserving `error_name` and `stack` from Error objects, and handling thrown strings).
/// - **Unresolved Promise (WouldBlock)**: returns a `JsError` with
///   `"returned unresolved promise"` message.
///
/// This is the single canonical path for Promise resolution across the entire
/// codebase — production probe resolution and test runner assertion evaluation
/// both route through it. Never formats rquickjs errors with Debug; always uses
/// `extract_js_error` for rejection details.
pub fn resolve_sync_or_promise<'js>(
    ctx: &Ctx<'js>,
    value: Value<'js>,
    stage: LifecycleStage,
) -> Result<Value<'js>, JsError> {
    if value.is_promise() {
        let promise = value
            .into_promise()
            .ok_or_else(|| JsError::new(stage, "returned invalid promise"))?;
        match promise.finish::<Value>() {
            Ok(val) => Ok(val),
            Err(Error::WouldBlock) => Err(JsError::new(stage, "returned unresolved promise")),
            Err(_) => Err(extract_js_error(ctx, stage)),
        }
    } else {
        Ok(value)
    }
}

/// Result of the override lifecycle execution.
#[derive(Debug)]
pub struct LifecycleResult {
    /// The (possibly AST-transformed) plugin script that was evaluated.
    pub script: String,
    /// Function names that were AST-patched (empty if no patch applied).
    pub patched_functions: Vec<String>,
}

/// Execute the full override lifecycle on a QuickJS context.
///
/// Steps:
/// 1. Optional `before_plugin` hook (called before plugin eval).
/// 2. AST-transform the plugin script if an override source is provided.
/// 3. Evaluate the (transformed) plugin script.
/// 4. If an override source exists, bootstrap `__openusage_override` via
///    `override_api::inject`, then evaluate the override script.
/// 5. Optional `after_lifecycle` hook (called after successful lifecycle).
///
/// Hook errors preserve their own stage (e.g. Harness, Setup, Execution)
/// via the `Hook` variant — they are NOT relabeled as PluginEval.
///
/// Returns the transformed script metadata, or the first stage error.
///
/// Production code calls this with no hooks for identical behavior.
///
/// # Panics
///
/// Panics if called from outside a QuickJS `Context::with` block (the `ctx`
/// parameter must be active).
#[allow(clippy::type_complexity)]
pub fn run_lifecycle(
    ctx: &Ctx<'_>,
    plugin_id: &str,
    plugin_source: &str,
    override_source: Option<&str>,
    before_plugin: Option<&dyn Fn(&Ctx<'_>) -> Result<(), JsError>>,
    after_lifecycle: Option<&dyn Fn(&Ctx<'_>) -> Result<(), JsError>>,
) -> Result<LifecycleResult, OverrideLifecycleError> {
    // 0. Before-plugin hook (harness/setup for tests)
    if let Some(hook) = before_plugin {
        hook(ctx).map_err(OverrideLifecycleError::Hook)?;
    }

    // 1. AST transform
    let transform_result =
        script_patch::transform_plugin_script(plugin_id, plugin_source, override_source).map_err(
            |msg| OverrideLifecycleError::Transform(JsError::new(LifecycleStage::Transform, msg)),
        )?;

    // 2. Evaluate the (transformed) plugin script
    ctx.eval::<(), _>(transform_result.script.as_bytes())
        .map_err(|_| {
            OverrideLifecycleError::PluginEval(JsError::from_caught(
                LifecycleStage::PluginEval,
                ctx,
            ))
        })?;

    // 3. If override source is present, bootstrap and evaluate
    if let Some(override_src) = override_source {
        override_api::inject(ctx, plugin_id).map_err(OverrideLifecycleError::OverrideApiInject)?;

        ctx.eval::<(), _>(override_src.as_bytes()).map_err(|_| {
            OverrideLifecycleError::OverrideEval(JsError::from_caught(
                LifecycleStage::OverrideEval,
                ctx,
            ))
        })?;

        // 3a. Process declarative function overrides from __openusage_function_overrides
        apply_function_overrides(ctx).map_err(OverrideLifecycleError::OverrideManifest)?;
    }

    // 4. After-lifecycle hook (execution/decode for tests)
    if let Some(hook) = after_lifecycle {
        hook(ctx).map_err(OverrideLifecycleError::Hook)?;
    }

    Ok(LifecycleResult {
        script: transform_result.script,
        patched_functions: transform_result.patched_functions,
    })
}

/// Process `globalThis.__openusage_function_overrides` after override eval.
///
/// Reads the declarative manifest from the JS context and validates each entry
/// against the strict whitelist, then installs it via the `__openusage_override` API.
///
/// # Whitelist
///
/// | Field    | Allowed values               |
/// |----------|------------------------------|
/// | `target` | `"discoverAccounts"`         |
/// | `mode`   | `"replace"`                  |
/// | `with`   | any callable global function |
///
/// # Behavior
///
/// - **Absent manifest** (`__openusage_function_overrides` is `null`, `undefined`,
///   or missing): no-op.
/// - **Malformed manifest** (missing required fields, non-object, non-array):
///   returns a detailed `JsError` with stage `OverrideManifest`.
/// - **Invalid target/mode** (outside whitelist): returns a detailed `JsError`.
/// - **Missing/non-callable `with` reference**: returns a detailed `JsError`.
///
/// # Delegation
///
/// Each valid entry calls `__openusage_override.replaceDiscoverAccounts(withFn)`,
/// which is the same codepath used by the direct imperative API — overriding
/// `plugin.discoverAccounts` to call `withFn(ctx, originalDiscoverAccounts)`.
pub fn apply_function_overrides(ctx: &Ctx<'_>) -> Result<(), JsError> {
    let globals = ctx.globals();

    // Check if the declarative manifest exists — absent is a no-op.
    let manifest_val: rquickjs::Value = match globals.get("__openusage_function_overrides") {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };

    if manifest_val.is_null() || manifest_val.is_undefined() {
        return Ok(());
    }

    let manifest_obj = manifest_val.into_object().ok_or_else(|| {
        JsError::new(
            LifecycleStage::OverrideManifest,
            "__openusage_function_overrides must be an object",
        )
    })?;

    let functions_val: rquickjs::Value = manifest_obj.get("functions").map_err(|_| {
        JsError::new(
            LifecycleStage::OverrideManifest,
            "__openusage_function_overrides missing 'functions' field",
        )
    })?;

    let functions_arr = functions_val.into_array().ok_or_else(|| {
        JsError::new(
            LifecycleStage::OverrideManifest,
            "__openusage_function_overrides.functions must be an array",
        )
    })?;

    let len = functions_arr.len();
    for i in 0..len {
        let entry_val: rquickjs::Value = functions_arr.get(i).map_err(|_| {
            JsError::new(
                LifecycleStage::OverrideManifest,
                format!("functions[{}] is invalid", i),
            )
        })?;

        let entry_obj = entry_val.into_object().ok_or_else(|| {
            JsError::new(
                LifecycleStage::OverrideManifest,
                format!("functions[{}] must be an object", i),
            )
        })?;

        // --- Validate `target` field (whitelist: only "discoverAccounts") ---
        let target: String = entry_obj.get("target").map_err(|_| {
            JsError::new(
                LifecycleStage::OverrideManifest,
                format!("functions[{}].target is required", i),
            )
        })?;

        if target != "discoverAccounts" {
            return Err(JsError::new(
                LifecycleStage::OverrideManifest,
                format!(
                    "functions[{}].target '{}' is not allowed (only 'discoverAccounts')",
                    i, target
                ),
            ));
        }

        // --- Validate `mode` field (whitelist: only "replace") ---
        let mode: String = entry_obj.get("mode").map_err(|_| {
            JsError::new(
                LifecycleStage::OverrideManifest,
                format!("functions[{}].mode is required", i),
            )
        })?;

        if mode != "replace" {
            return Err(JsError::new(
                LifecycleStage::OverrideManifest,
                format!(
                    "functions[{}].mode '{}' is not allowed (only 'replace')",
                    i, mode
                ),
            ));
        }

        // --- Validate `with` field (must reference a callable global) ---
        let with_name: String = entry_obj.get("with").map_err(|_| {
            JsError::new(
                LifecycleStage::OverrideManifest,
                format!("functions[{}].with is required", i),
            )
        })?;

        // Look up the function by name on globalThis
        let with_val: rquickjs::Value = globals.get(&with_name).map_err(|_| {
            JsError::new(
                LifecycleStage::OverrideManifest,
                format!(
                    "functions[{}].with '{}' not found on globalThis",
                    i, with_name
                ),
            )
        })?;

        // In QuickJS, non-existent globals return undefined rather than an error
        if with_val.is_undefined() || with_val.is_null() {
            return Err(JsError::new(
                LifecycleStage::OverrideManifest,
                format!(
                    "functions[{}].with '{}' not found on globalThis",
                    i, with_name
                ),
            ));
        }

        if !with_val.is_function() {
            return Err(JsError::new(
                LifecycleStage::OverrideManifest,
                format!("functions[{}].with '{}' is not a function", i, with_name),
            ));
        }
        let with_fn = with_val.into_function().ok_or_else(|| {
            JsError::new(
                LifecycleStage::OverrideManifest,
                format!("functions[{}].with '{}' is not callable", i, with_name),
            )
        })?;

        // --- Install via existing replaceDiscoverAccounts mechanism ---
        override_api::install_discover_accounts_override(ctx, with_fn)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::{Context, Runtime};

    #[test]
    fn resolve_sync_value_returns_as_is() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let val: Value = ctx.eval("42").expect("eval");
            let result = resolve_sync_or_promise(&ctx, val, LifecycleStage::Execution);
            assert!(result.is_ok());
            let v = result.unwrap();
            assert!(v.is_number());
            assert_eq!(v.as_number().unwrap(), 42.0);
        });
    }

    #[test]
    fn resolve_fulfilled_promise_returns_value() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let val: Value = ctx
                .eval(
                    r#"
                (function() {
                    return Promise.resolve("hello from promise");
                })();
                "#,
                )
                .expect("eval");
            let result = resolve_sync_or_promise(&ctx, val, LifecycleStage::Execution);
            assert!(result.is_ok());
            let v = result.unwrap();
            assert!(v.is_string());
            assert_eq!(
                v.as_string().unwrap().to_string().unwrap(),
                "hello from promise"
            );
        });
    }

    #[test]
    fn resolve_rejected_string_returns_js_error() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let val: Value = ctx
                .eval(
                    r#"
                (function() {
                    return Promise.reject("custom rejection");
                })();
                "#,
                )
                .expect("eval");
            let result = resolve_sync_or_promise(&ctx, val, LifecycleStage::Execution);
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert_eq!(err.message, "custom rejection");
            assert_eq!(err.error_name, None);
        });
    }

    #[test]
    fn resolve_rejected_error_object_includes_details() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let val: Value = ctx
                .eval(
                    r#"
                (function() {
                    var e = new Error("detailed failure");
                    e.name = "TypeError";
                    return Promise.reject(e);
                })();
                "#,
                )
                .expect("eval");
            let result = resolve_sync_or_promise(&ctx, val, LifecycleStage::Execution);
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert_eq!(err.message, "detailed failure");
            assert_eq!(err.error_name.as_deref(), Some("TypeError"));
            assert!(
                err.stack.is_some(),
                "rejected Error object should carry a stack trace"
            );
        });
    }

    #[test]
    fn resolve_unresolved_promise_returns_would_block_error() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let val: Value = ctx
                .eval(
                    r#"
                (function() {
                    // Create a never-settled promise (no resolve/reject called)
                    return new Promise(function() {});
                })();
                "#,
                )
                .expect("eval");
            let result = resolve_sync_or_promise(&ctx, val, LifecycleStage::Execution);
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(
                err.message.contains("unresolved"),
                "expected unresolved promise message, got: {}",
                err.message
            );
        });
    }

    // ---------------------------------------------------------------------------
    // Declarative function override manifest tests
    // ---------------------------------------------------------------------------

    /// Helper: run the full lifecycle with a plugin that has discoverAccounts,
    /// an override script that sets __openusage_function_overrides, and return
    /// the LifecycleResult or error.
    fn run_with_manifest_override(
        plugin_source: &str,
        override_source: &str,
    ) -> Result<LifecycleResult, OverrideLifecycleError> {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            run_lifecycle(
                &ctx,
                "test-plugin",
                plugin_source,
                Some(override_source),
                None,
                None,
            )
        })
    }

    #[test]
    fn valid_manifest_replaces_discover_accounts() {
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "original" }]; }
            };
        "#;
        let override_js = r#"
            function myDiscover(ctx, orig) {
                return [{ id: "replaced" }];
            }
            globalThis.__openusage_function_overrides = {
                functions: [{ target: "discoverAccounts", mode: "replace", with: "myDiscover" }]
            };
        "#;
        let result = run_with_manifest_override(plugin, override_js);
        assert!(
            result.is_ok(),
            "lifecycle should succeed: {:?}",
            result.err()
        );
    }

    #[test]
    fn valid_manifest_replaces_discovery_call() {
        // Verify the replacement actually takes effect by calling discoverAccounts
        // after the lifecycle completes.
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "original" }]; }
            };
        "#;
        let override_js = r#"
            function myDiscover(ctx, orig) {
                return [{ id: "replaced" }];
            }
            globalThis.__openusage_function_overrides = {
                functions: [{ target: "discoverAccounts", mode: "replace", with: "myDiscover" }]
            };
        "#;
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let result = run_lifecycle(&ctx, "test-plugin", plugin, Some(override_js), None, None);
            assert!(result.is_ok(), "lifecycle failed: {:?}", result.err());

            // Verify discoverAccounts was replaced
            let _plugin_obj: rquickjs::Object = ctx
                .globals()
                .get("__openusage_plugin")
                .expect("plugin object");
            let accounts: String = ctx
                .eval(
                    r#"
                    (function() {
                        var p = globalThis.__openusage_plugin;
                        var accs = p.discoverAccounts({});
                        return JSON.stringify(accs);
                    })();
                    "#,
                )
                .expect("eval accounts");
            let parsed: serde_json::Value =
                serde_json::from_str(&accounts).expect("parse accounts");
            let arr = parsed.as_array().expect("accounts array");
            assert_eq!(arr.len(), 1);
            assert_eq!(arr[0]["id"], "replaced");
        });
    }

    #[test]
    fn absent_manifest_is_noop() {
        // Override script without __openusage_function_overrides — lifecycle
        // should succeed and discoverAccounts should remain unchanged.
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "original" }]; }
            };
        "#;
        let override_js = r#"
            // No __openusage_function_overrides set — just a plain override script.
        "#;
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let result = run_lifecycle(&ctx, "test-plugin", plugin, Some(override_js), None, None);
            assert!(result.is_ok(), "lifecycle failed: {:?}", result.err());

            // discoverAccounts should still be the original
            let accounts: String = ctx
                .eval(
                    r#"
                    (function() {
                        var p = globalThis.__openusage_plugin;
                        var accs = p.discoverAccounts({});
                        return JSON.stringify(accs);
                    })();
                    "#,
                )
                .expect("eval accounts");
            let parsed: serde_json::Value =
                serde_json::from_str(&accounts).expect("parse accounts");
            let arr = parsed.as_array().expect("accounts array");
            assert_eq!(arr.len(), 1);
            assert_eq!(arr[0]["id"], "original");
        });
    }

    #[test]
    fn null_manifest_is_noop() {
        // Manifest set to null — should be treated as absent.
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "original" }]; }
            };
        "#;
        let override_js = r#"
            globalThis.__openusage_function_overrides = null;
        "#;
        let result = run_with_manifest_override(plugin, override_js);
        assert!(
            result.is_ok(),
            "lifecycle should succeed with null manifest"
        );
    }

    #[test]
    fn unknown_target_fails_visibly() {
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "test" }]; }
            };
        "#;
        let override_js = r#"
            function myFn(ctx, orig) { return [{ id: "x" }]; }
            globalThis.__openusage_function_overrides = {
                functions: [{ target: "probe", mode: "replace", with: "myFn" }]
            };
        "#;
        let result = run_with_manifest_override(plugin, override_js);
        assert!(result.is_err(), "lifecycle should fail with unknown target");
        let err = result.unwrap_err();
        match err {
            OverrideLifecycleError::OverrideManifest(js_err) => {
                assert!(
                    js_err.message.contains("not allowed"),
                    "message should mention target not allowed, got: {}",
                    js_err.message
                );
                assert_eq!(js_err.stage, LifecycleStage::OverrideManifest);
            }
            other => panic!("expected OverrideManifest error, got: {:?}", other),
        }
    }

    #[test]
    fn unknown_mode_fails_visibly() {
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "test" }]; }
            };
        "#;
        let override_js = r#"
            function myFn(ctx, orig) { return [{ id: "x" }]; }
            globalThis.__openusage_function_overrides = {
                functions: [{ target: "discoverAccounts", mode: "wrap", with: "myFn" }]
            };
        "#;
        let result = run_with_manifest_override(plugin, override_js);
        assert!(result.is_err(), "lifecycle should fail with unknown mode");
        let err = result.unwrap_err();
        match err {
            OverrideLifecycleError::OverrideManifest(js_err) => {
                assert!(
                    js_err.message.contains("not allowed"),
                    "message should mention mode not allowed, got: {}",
                    js_err.message
                );
            }
            other => panic!("expected OverrideManifest error, got: {:?}", other),
        }
    }

    #[test]
    fn missing_with_fails_visibly() {
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "test" }]; }
            };
        "#;
        let override_js = r#"
            globalThis.__openusage_function_overrides = {
                functions: [{ target: "discoverAccounts", mode: "replace" }]
            };
        "#;
        let result = run_with_manifest_override(plugin, override_js);
        assert!(result.is_err(), "lifecycle should fail without 'with'");
        let err = result.unwrap_err();
        match err {
            OverrideLifecycleError::OverrideManifest(js_err) => {
                assert!(
                    js_err.message.contains("with"),
                    "message should mention 'with', got: {}",
                    js_err.message
                );
            }
            other => panic!("expected OverrideManifest error, got: {:?}", other),
        }
    }

    #[test]
    fn nonexistent_with_function_fails_visibly() {
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "test" }]; }
            };
        "#;
        let override_js = r#"
            globalThis.__openusage_function_overrides = {
                functions: [{ target: "discoverAccounts", mode: "replace", with: "nonExistentFn" }]
            };
        "#;
        let result = run_with_manifest_override(plugin, override_js);
        assert!(
            result.is_err(),
            "lifecycle should fail with unknown function"
        );
        let err = result.unwrap_err();
        match err {
            OverrideLifecycleError::OverrideManifest(js_err) => {
                assert!(
                    js_err.message.contains("not found"),
                    "message should mention 'not found', got: {}",
                    js_err.message
                );
            }
            other => panic!("expected OverrideManifest error, got: {:?}", other),
        }
    }

    #[test]
    fn non_callable_with_fails_visibly() {
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "test" }]; }
            };
        "#;
        let override_js = r#"
            var notAFn = "i am a string, not a function";
            globalThis.__openusage_function_overrides = {
                functions: [{ target: "discoverAccounts", mode: "replace", with: "notAFn" }]
            };
        "#;
        let result = run_with_manifest_override(plugin, override_js);
        assert!(result.is_err(), "lifecycle should fail with non-callable");
        let err = result.unwrap_err();
        match err {
            OverrideLifecycleError::OverrideManifest(js_err) => {
                assert!(
                    js_err.message.contains("not a function"),
                    "message should mention 'not a function', got: {}",
                    js_err.message
                );
            }
            other => panic!("expected OverrideManifest error, got: {:?}", other),
        }
    }

    #[test]
    fn manifest_not_an_object_fails_visibly() {
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "test" }]; }
            };
        "#;
        let override_js = r#"
            globalThis.__openusage_function_overrides = "string-not-object";
        "#;
        let result = run_with_manifest_override(plugin, override_js);
        assert!(
            result.is_err(),
            "lifecycle should fail with non-object manifest"
        );
        let err = result.unwrap_err();
        match err {
            OverrideLifecycleError::OverrideManifest(js_err) => {
                assert!(
                    js_err.message.contains("must be an object"),
                    "message should mention object, got: {}",
                    js_err.message
                );
            }
            other => panic!("expected OverrideManifest error, got: {:?}", other),
        }
    }

    #[test]
    fn functions_not_an_array_fails_visibly() {
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "test" }]; }
            };
        "#;
        let override_js = r#"
            globalThis.__openusage_function_overrides = {
                functions: "not-an-array"
            };
        "#;
        let result = run_with_manifest_override(plugin, override_js);
        assert!(
            result.is_err(),
            "lifecycle should fail with non-array functions"
        );
        let err = result.unwrap_err();
        match err {
            OverrideLifecycleError::OverrideManifest(js_err) => {
                assert!(
                    js_err.message.contains("must be an array"),
                    "message should mention array, got: {}",
                    js_err.message
                );
            }
            other => panic!("expected OverrideManifest error, got: {:?}", other),
        }
    }

    #[test]
    fn existing_direct_api_remains_compatible() {
        // The existing imperative __openusage_override.replaceDiscoverAccounts
        // must still work after the lifecycle changes.
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "original" }]; }
            };
        "#;
        let override_js = r#"
            // No manifest — just use the direct API
            globalThis.__openusage_override.replaceDiscoverAccounts(function(ctx, orig) {
                return [{ id: "direct-api" }];
            });
        "#;
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let result = run_lifecycle(&ctx, "test-plugin", plugin, Some(override_js), None, None);
            assert!(result.is_ok(), "lifecycle failed: {:?}", result.err());

            let accounts: String = ctx
                .eval(
                    r#"
                    (function() {
                        var p = globalThis.__openusage_plugin;
                        var accs = p.discoverAccounts({});
                        return JSON.stringify(accs);
                    })();
                    "#,
                )
                .expect("eval accounts");
            let parsed: serde_json::Value =
                serde_json::from_str(&accounts).expect("parse accounts");
            let arr = parsed.as_array().expect("accounts array");
            assert_eq!(arr.len(), 1);
            assert_eq!(arr[0]["id"], "direct-api");
        });
    }

    #[test]
    fn manifest_without_override_does_nothing() {
        // When override_source is None, the manifest in the plugin script itself
        // should not be processed (manifest is only processed after override eval).
        let plugin = r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "original" }]; }
            };
            globalThis.__openusage_function_overrides = {
                functions: [{ target: "discoverAccounts", mode: "replace", with: "nonexistent" }]
            };
        "#;
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            // No override source — manifest in plugin script is ignored
            let result = run_lifecycle(&ctx, "test-plugin", plugin, None, None, None);
            assert!(result.is_ok(), "lifecycle should succeed without override");

            // discoverAccounts should remain the original
            let accounts: String = ctx
                .eval(
                    r#"
                    (function() {
                        var p = globalThis.__openusage_plugin;
                        var accs = p.discoverAccounts({});
                        return JSON.stringify(accs);
                    })();
                    "#,
                )
                .expect("eval accounts");
            let parsed: serde_json::Value =
                serde_json::from_str(&accounts).expect("parse accounts");
            let arr = parsed.as_array().expect("accounts array");
            assert_eq!(arr.len(), 1);
            assert_eq!(arr[0]["id"], "original");
        });
    }
}
