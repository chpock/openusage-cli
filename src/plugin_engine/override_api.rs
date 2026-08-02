/// Override API bootstrap — single source of truth for `__openusage_override`.
///
/// The bootstrap JS (`override_api_bootstrap.js`) defines a function that
/// accepts `pluginId` and sets up `globalThis.__openusage_override` with
/// `replaceProbe` / `wrapProbe` / `resetProbe` / `replaceDiscoverAccounts` /
/// `wrapDiscoverAccounts` / `resetDiscoverAccounts` helpers.
///
/// # Invariants
/// - The bootstrap function MUST be called AFTER the plugin entry script has
///   been evaluated (so `__openusage_plugin` and `probe()` exist).
/// - It MUST be called BEFORE the override script is evaluated (so the
///   override can use `__openusage_override`).
use crate::plugin_engine::override_lifecycle::{self, JsError, LifecycleStage};
use rquickjs::{Ctx, Function};

/// Bootstrap JS source — embedded at compile time.
const BOOTSTRAP_JS: &str = include_str!("override_api_bootstrap.js");

/// Inject the `__openusage_override` API into the QuickJS context.
///
/// Evaluates the bootstrap JS (which returns a function accepting `pluginId`),
/// then calls it with the given `plugin_id` to install the override helpers
/// on `globalThis.__openusage_override`.
///
/// Returns a detailed `JsError` on failure, preserving Error-object name,
/// message, and stack trace when available.
pub fn inject(ctx: &Ctx<'_>, plugin_id: &str) -> Result<(), JsError> {
    let bootstrap_fn: Function = ctx.eval(BOOTSTRAP_JS.as_bytes()).map_err(|_| {
        override_lifecycle::extract_js_error(ctx, LifecycleStage::OverrideBootstrap)
    })?;

    bootstrap_fn
        .call::<(String,), ()>((String::from(plugin_id),))
        .map_err(|_| {
            override_lifecycle::extract_js_error(ctx, LifecycleStage::OverrideBootstrap)
        })?;

    Ok(())
}

/// Install a `discoverAccounts` override via the existing `replaceDiscoverAccounts`
/// mechanism on `__openusage_override`.
///
/// Looks up `__openusage_override.replaceDiscoverAccounts` on the global scope
/// and calls it with the given callback function. The callback receives
/// `(ctx, originalDiscoverAccounts)` — matching the existing override API contract.
///
/// Returns a detailed `JsError` on failure (missing override object, missing
/// replaceDiscoverAccounts, or a JS exception from the call itself).
pub fn install_discover_accounts_override<'js>(
    ctx: &Ctx<'js>,
    callback: rquickjs::Function<'js>,
) -> Result<(), JsError> {
    let globals = ctx.globals();
    let override_obj: rquickjs::Object = globals.get("__openusage_override").map_err(|_| {
        JsError::new(
            LifecycleStage::OverrideManifest,
            "__openusage_override not found",
        )
    })?;

    let replace_fn: rquickjs::Function<'js> =
        override_obj.get("replaceDiscoverAccounts").map_err(|_| {
            JsError::new(
                LifecycleStage::OverrideManifest,
                "replaceDiscoverAccounts not found on __openusage_override",
            )
        })?;

    replace_fn
        .call::<(rquickjs::Function<'js>,), ()>((callback,))
        .map_err(|_| override_lifecycle::extract_js_error(ctx, LifecycleStage::OverrideManifest))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rquickjs::{Context, Object, Runtime, Value};
    use serde_json::Value as JsonValue;

    /// Build a bare `__openusage_plugin` with a no-op `probe`.
    fn setup_bare_plugin(ctx: &Ctx<'_>) {
        ctx.eval::<(), _>(
            r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; }
            };
            "#
            .as_bytes(),
        )
        .expect("setup bare plugin");
    }

    fn setup_with_discovery(ctx: &Ctx<'_>) {
        ctx.eval::<(), _>(
            r#"
            globalThis.__openusage_plugin = {
                probe: function() { return { lines: [] }; },
                discoverAccounts: function() { return [{ id: "default" }]; }
            };
            "#
            .as_bytes(),
        )
        .expect("setup plugin with discovery");
    }

    fn eval_json(ctx: &Ctx<'_>, source: &str) -> JsonValue {
        let json: String = ctx.eval(source.as_bytes()).expect("eval");
        serde_json::from_str(&json).expect("parse json")
    }

    #[test]
    fn inject_sets_override_object() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let globals = ctx.globals();
            let override_obj: Object = globals
                .get("__openusage_override")
                .expect("__openusage_override must exist");
            let plugin_id: String = override_obj.get("pluginId").expect("pluginId must be set");
            assert_eq!(plugin_id, "test-plugin");
        });
    }

    #[test]
    fn inject_sets_replace_probe() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let globals = ctx.globals();
            let override_obj: Object = globals
                .get("__openusage_override")
                .expect("__openusage_override must exist");
            let replace_probe: Value = override_obj
                .get("replaceProbe")
                .expect("replaceProbe must exist");
            assert!(
                replace_probe.is_function(),
                "replaceProbe must be a function"
            );
        });
    }

    #[test]
    fn inject_sets_wrap_probe() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let globals = ctx.globals();
            let override_obj: Object = globals
                .get("__openusage_override")
                .expect("__openusage_override must exist");
            let wrap_probe: Value = override_obj.get("wrapProbe").expect("wrapProbe must exist");
            assert!(wrap_probe.is_function(), "wrapProbe must be a function");
        });
    }

    #[test]
    fn inject_sets_reset_probe() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let globals = ctx.globals();
            let override_obj: Object = globals
                .get("__openusage_override")
                .expect("__openusage_override must exist");
            let reset_probe: Value = override_obj
                .get("resetProbe")
                .expect("resetProbe must exist");
            assert!(reset_probe.is_function(), "resetProbe must be a function");
        });
    }

    #[test]
    fn inject_sets_original_probe() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let globals = ctx.globals();
            let override_obj: Object = globals
                .get("__openusage_override")
                .expect("__openusage_override must exist");
            let original_probe: Value = override_obj
                .get("originalProbe")
                .expect("originalProbe must exist");
            assert!(
                original_probe.is_function(),
                "originalProbe must be a function"
            );
        });
    }

    #[test]
    fn inject_sets_discover_accounts_helpers_when_present() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_with_discovery(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let globals = ctx.globals();
            let override_obj: Object = globals
                .get("__openusage_override")
                .expect("__openusage_override must exist");

            let original_da: Value = override_obj
                .get("originalDiscoverAccounts")
                .expect("originalDiscoverAccounts must exist");
            assert!(
                original_da.is_function(),
                "originalDiscoverAccounts must be a function"
            );

            let replace_da: Value = override_obj
                .get("replaceDiscoverAccounts")
                .expect("replaceDiscoverAccounts must exist");
            assert!(
                replace_da.is_function(),
                "replaceDiscoverAccounts must be a function"
            );

            let wrap_da: Value = override_obj
                .get("wrapDiscoverAccounts")
                .expect("wrapDiscoverAccounts must exist");
            assert!(
                wrap_da.is_function(),
                "wrapDiscoverAccounts must be a function"
            );

            let reset_da: Value = override_obj
                .get("resetDiscoverAccounts")
                .expect("resetDiscoverAccounts must exist");
            assert!(
                reset_da.is_function(),
                "resetDiscoverAccounts must be a function"
            );
        });
    }

    #[test]
    fn inject_sets_discover_accounts_helpers_null_when_absent() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let globals = ctx.globals();
            let override_obj: Object = globals
                .get("__openusage_override")
                .expect("__openusage_override must exist");

            let original_da: Value = override_obj
                .get("originalDiscoverAccounts")
                .expect("originalDiscoverAccounts must exist");
            assert!(
                original_da.is_null(),
                "originalDiscoverAccounts must be null when not present"
            );

            let replace_da: Value = override_obj
                .get("replaceDiscoverAccounts")
                .expect("replaceDiscoverAccounts must exist");
            assert!(
                replace_da.is_function(),
                "replaceDiscoverAccounts must be a function"
            );

            let wrap_da: Value = override_obj
                .get("wrapDiscoverAccounts")
                .expect("wrapDiscoverAccounts must exist");
            assert!(
                wrap_da.is_function(),
                "wrapDiscoverAccounts must be a function"
            );

            let reset_da: Value = override_obj
                .get("resetDiscoverAccounts")
                .expect("resetDiscoverAccounts must exist");
            assert!(
                reset_da.is_function(),
                "resetDiscoverAccounts must be a function"
            );
        });
    }

    #[test]
    fn replace_probe_replaces_probe_function() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let parsed = eval_json(
                &ctx,
                r#"
                (function() {
                    var over = globalThis.__openusage_override;
                    var plugin = globalThis.__openusage_plugin;
                    var oldProbe = plugin.probe;
                    over.replaceProbe(function(ctx, original) {
                        return { lines: [{ type: "text", label: "Replaced", value: "yes" }] };
                    });
                    var result = plugin.probe({});
                    return JSON.stringify({
                        probeReplaced: plugin.probe !== oldProbe,
                        result: result
                    });
                })();
                "#,
            );
            assert_eq!(parsed["probeReplaced"], JsonValue::Bool(true));
            assert_eq!(parsed["result"]["lines"][0]["label"], "Replaced");
        });
    }

    #[test]
    fn wrap_probe_wraps_and_calls_original() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let parsed = eval_json(
                &ctx,
                r#"
                (function() {
                    var over = globalThis.__openusage_override;
                    var plugin = globalThis.__openusage_plugin;
                    var oldProbe = plugin.probe;
                    over.wrapProbe(function(ctx, previous, original) {
                        var origResult = original(ctx);
                        origResult.lines.push({ type: "text", label: "Wrapped", value: "yes" });
                        return origResult;
                    });
                    var result = plugin.probe({});
                    return JSON.stringify({
                        probeWrapped: plugin.probe !== oldProbe,
                        result: result
                    });
                })();
                "#,
            );
            assert_eq!(parsed["probeWrapped"], JsonValue::Bool(true));
            let lines = parsed["result"]["lines"].as_array().expect("lines array");
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0]["label"], "Wrapped");
        });
    }

    #[test]
    fn reset_probe_restores_original() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let parsed = eval_json(
                &ctx,
                r#"
                (function() {
                    var over = globalThis.__openusage_override;
                    var plugin = globalThis.__openusage_plugin;
                    var original = plugin.probe;
                    over.replaceProbe(function(ctx, orig) {
                        return { lines: [{ type: "text", label: "Replaced", value: "yes" }] };
                    });
                    var replaced = plugin.probe;
                    var restored = over.resetProbe();
                    // After reset, plugin.probe should produce the same result as original
                    var resetResult = plugin.probe({});
                    var origResult = original({});
                    return JSON.stringify({
                        originalIsFunction: typeof original === "function",
                        replacedIsDifferent: replaced !== original,
                        probeIsFunctionAfterReset: typeof plugin.probe === "function",
                        resetProducesSameResult: JSON.stringify(resetResult) === JSON.stringify(origResult)
                    });
                })();
                "#,
            );
            assert_eq!(parsed["originalIsFunction"], JsonValue::Bool(true));
            assert_eq!(parsed["replacedIsDifferent"], JsonValue::Bool(true));
            assert_eq!(parsed["probeIsFunctionAfterReset"], JsonValue::Bool(true));
            assert_eq!(parsed["resetProducesSameResult"], JsonValue::Bool(true));
        });
    }

    #[test]
    fn replace_probe_rejects_non_function() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let result: String = ctx
                .eval(
                    r#"
                (function() {
                    try {
                        globalThis.__openusage_override.replaceProbe("not-a-function");
                        return JSON.stringify({ ok: true });
                    } catch (e) {
                        return JSON.stringify({ ok: false, error: String(e) });
                    }
                })();
                "#
                    .as_bytes(),
                )
                .expect("eval");
            let parsed: JsonValue = serde_json::from_str(&result).expect("parse result");
            assert_eq!(parsed["ok"], JsonValue::Bool(false));
            let error = parsed["error"].as_str().unwrap_or_default();
            assert!(
                error.contains("expects a function"),
                "error should mention function requirement, got: {}",
                error
            );
        });
    }

    #[test]
    fn wrap_probe_rejects_non_function() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let result: String = ctx
                .eval(
                    r#"
                (function() {
                    try {
                        globalThis.__openusage_override.wrapProbe("not-a-function");
                        return JSON.stringify({ ok: true });
                    } catch (e) {
                        return JSON.stringify({ ok: false, error: String(e) });
                    }
                })();
                "#
                    .as_bytes(),
                )
                .expect("eval");
            let parsed: JsonValue = serde_json::from_str(&result).expect("parse result");
            assert_eq!(parsed["ok"], JsonValue::Bool(false));
            let error = parsed["error"].as_str().unwrap_or_default();
            assert!(
                error.contains("expects a function"),
                "error should mention function requirement, got: {}",
                error
            );
        });
    }

    #[test]
    fn errors_when_no_plugin_object() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let result = inject(&ctx, "test-plugin");
            assert!(result.is_err(), "inject should fail without plugin object");
        });
    }

    #[test]
    fn errors_when_no_probe_function() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
                globalThis.__openusage_plugin = { id: "test" };
                "#
                .as_bytes(),
            )
            .expect("setup plugin without probe");
            let result = inject(&ctx, "test-plugin");
            assert!(result.is_err(), "inject should fail without probe function");
        });
    }

    #[test]
    fn replace_discover_accounts_replaces_function() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_with_discovery(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let parsed = eval_json(
                &ctx,
                r#"
                (function() {
                    var over = globalThis.__openusage_override;
                    var plugin = globalThis.__openusage_plugin;
                    over.replaceDiscoverAccounts(function(ctx, original) {
                        return [{ id: "custom-account" }];
                    });
                    var accounts = plugin.discoverAccounts({});
                    return JSON.stringify({
                        accounts: accounts
                    });
                })();
                "#,
            );
            let accounts = parsed["accounts"].as_array().expect("accounts array");
            assert_eq!(accounts.len(), 1);
            assert_eq!(accounts[0]["id"], "custom-account");
        });
    }

    #[test]
    fn reset_discover_accounts_restores_null_when_no_original() {
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            setup_bare_plugin(&ctx);
            inject(&ctx, "test-plugin").expect("inject override API");

            let parsed = eval_json(
                &ctx,
                r#"
                (function() {
                    var over = globalThis.__openusage_override;
                    var restored = over.resetDiscoverAccounts();
                    return JSON.stringify({
                        restored: restored
                    });
                })();
                "#,
            );
            assert!(parsed["restored"].is_null());
        });
    }

    #[test]
    fn inject_bootstrap_error_preserves_error_object_details() {
        // Verify that a JS Error thrown during bootstrap eval preserves
        // name, message, and stack trace in the returned JsError.
        // Use a throwing getter on probe to trigger bootstrap's
        // typeof-plugin.probe check.
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
                globalThis.__openusage_plugin = {};
                Object.defineProperty(globalThis.__openusage_plugin, 'probe', {
                    get: function() { throw new TypeError("probe access failure"); }
                });
                "#
                .as_bytes(),
            )
            .expect("setup");
            let result = inject(&ctx, "test-plugin");
            assert!(result.is_err(), "inject should fail");
            let js_err = result.unwrap_err();
            assert_eq!(
                js_err.error_name.as_deref(),
                Some("TypeError"),
                "expected TypeError name, got {:?}",
                js_err.error_name
            );
            assert!(
                js_err.message.contains("probe access failure"),
                "message should contain error text, got: {}",
                js_err.message
            );
            assert!(
                js_err.stack.is_some(),
                "stack should be present for Error object"
            );
            assert_eq!(js_err.stage, LifecycleStage::OverrideBootstrap);
        });
    }

    #[test]
    fn inject_bootstrap_error_preserves_thrown_string() {
        // Verify that a thrown string during bootstrap preserves the
        // string as the JsError message (no error_name or stack).
        // __openusage_plugin not set -> bootstrap throws string.
        let rt = Runtime::new().expect("runtime");
        let ctx = Context::full(&rt).expect("context");
        ctx.with(|ctx| {
            let result = inject(&ctx, "test-plugin");
            assert!(result.is_err(), "inject should fail");
            let js_err = result.unwrap_err();
            assert!(
                js_err.message.contains("missing __openusage_plugin"),
                "message should contain 'missing __openusage_plugin', got: {}",
                js_err.message
            );
            assert!(
                js_err.error_name.is_none(),
                "no error_name for string throw"
            );
            assert!(js_err.stack.is_none(), "no stack for string throw");
            assert_eq!(js_err.stage, LifecycleStage::OverrideBootstrap);
        });
    }
}
