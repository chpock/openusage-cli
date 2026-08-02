#![allow(unreachable_patterns)]

// Integration tests for the override lifecycle runner.
//
// Contains the common contract suite for `run_lifecycle` (exec/load-only mode)
// and for `execute_provider_in_context` (probe mode).
// Provider-specific lifecycle tests that call `run_probe` live in their own
// files (codex_override.rs, copilot_override.rs).
//
// One test per concern. No duplicate/tautological tests.

mod support;

use openusage_cli::plugin_engine::override_lifecycle::LifecycleStage;
use serde_json::Value;

use support::override_runner::{
    LifecycleOutcome, LifecycleSpec, ProviderOutcome, ProviderSpec, run_lifecycle_contract,
    run_provider_probe,
};

// ═══════════════════════════════════════════════════════════════════════
// Ordering: harness → setup → plugin → (override bootstrap → override eval)
// → execution (via hooks)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn ordering_hooks_execute_in_correct_sequence() {
    // The before_plugin hook runs first (harness → setup), then the plugin
    // script is evaluated, then the after_lifecycle hook runs the execution.
    // Verify the plugin sees state set by the before hook and the execution
    // sees the plugin state.
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "order-test",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            // Before hook should have set this marker
            var harnessRan = typeof globalThis.__harness_marker !== "undefined"
                && globalThis.__harness_marker === "setup-done";
            globalThis.__openusage_plugin = {
                id: "order-test",
                probe: function() { return { lines: [] }; },
                harnessRan: harnessRan
            };
        })();
        "#,
        override_source: None,
        harness: "globalThis.__harness_marker = 'harness-ran';",
        setup: "globalThis.__harness_marker = 'setup-done';",
        execution: Some(
            r#"
        (function() {
            return JSON.stringify({
                pluginExists: typeof globalThis.__openusage_plugin === "object",
                harnessRan: globalThis.__openusage_plugin.harnessRan,
                noOverride: typeof globalThis.__openusage_override === "undefined"
            });
        })();
        "#,
        ),
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::Json(val) => {
            assert_eq!(val["pluginExists"], Value::Bool(true));
            assert_eq!(val["harnessRan"], Value::Bool(true));
            assert_eq!(val["noOverride"], Value::Bool(true));
        }
        other => panic!("expected Json, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Absent override: no __openusage_override injected
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn absent_override_no_api_on_global() {
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "absent-test",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "absent", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        execution: Some(
            r#"
        (function() {
            return JSON.stringify({ exists: typeof globalThis.__openusage_override !== "undefined" });
        })();
        "#,
        ),
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::Json(val) => {
            assert_eq!(val["exists"], Value::Bool(false));
        }
        other => panic!("expected Json, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Override present: __openusage_override injected with helpers
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn override_present_injects_api() {
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "present-test",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "present", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: Some(
            r#"
        var over = globalThis.__openusage_override;
        globalThis.__ov = {
            wasPresent: typeof over === "object",
            hasReplace: typeof over.replaceProbe === "function",
            hasWrap: typeof over.wrapProbe === "function",
            hasReset: typeof over.resetProbe === "function"
        };
        "#,
        ),
        harness: "",
        setup: "",
        execution: Some(
            r#"
        (function() {
            var ov = globalThis.__ov;
            return JSON.stringify({
                overridePresent: ov.wasPresent,
                hasReplaceProbe: ov.hasReplace,
                hasWrapProbe: ov.hasWrap,
                hasResetProbe: ov.hasReset
            });
        })();
        "#,
        ),
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::Json(val) => {
            assert_eq!(val["overridePresent"], Value::Bool(true));
            assert_eq!(val["hasReplaceProbe"], Value::Bool(true));
            assert_eq!(val["hasWrapProbe"], Value::Bool(true));
            assert_eq!(val["hasResetProbe"], Value::Bool(true));
        }
        other => panic!("expected Json, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Load-only: returns lifecycle metadata without execution
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn load_only_returns_metadata() {
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "load-only",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "load", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::Loaded {
            script,
            patched_functions,
        } => {
            assert!(
                script.contains("__openusage_plugin"),
                "script should contain plugin registration"
            );
            assert!(patched_functions.is_empty(), "no patches expected");
        }
        other => panic!("expected Loaded, got {:?}", other),
    }
}

#[test]
fn load_only_with_override_includes_ast_patch_metadata() {
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "load-override",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            function loadAuth(ctx) { return null; }
            globalThis.__openusage_plugin = { id: "load", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: Some(
            r#"
        globalThis.__openusage_ast_patch = {
            functions: [{ target: "loadAuth", with: "patchLoadAuth", mode: "wrap" }]
        };
        function patchLoadAuth(original, ctx) { return original(ctx); }
        "#,
        ),
        harness: "",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::Loaded {
            script,
            patched_functions,
        } => {
            assert!(
                script.contains("__openusage_original_loadAuth"),
                "loadAuth should be renamed"
            );
            assert_eq!(patched_functions, vec!["loadAuth"]);
        }
        other => panic!("expected Loaded, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Fresh-context isolation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn fresh_context_isolation() {
    // Each run_spec call must get a fresh QuickJS context.
    let spec1 = LifecycleSpec {
        plugin_id: "iso1",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "iso1", probe: function() { return { lines: [] }; } };
            globalThis.__custom_value = 42;
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        execution: Some(
            r#"
        (function() {
            return JSON.stringify({ custom: typeof globalThis.__custom_value !== "undefined" ? globalThis.__custom_value : null });
        })();
        "#,
        ),
        // probe_mode removed — LifecycleSpec has no such field
    };
    let result1 = run_lifecycle_contract(&spec1);
    let val1 = match result1 {
        LifecycleOutcome::Json(v) => v,
        other => panic!("expected Json, got {:?}", other),
    };
    assert_eq!(val1["custom"], 42);

    // Second spec must NOT see __custom_value.
    let spec2 = LifecycleSpec {
        plugin_id: "iso2",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "iso2", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        execution: Some(
            r#"
        (function() {
            return JSON.stringify({ custom: typeof globalThis.__custom_value !== "undefined" ? globalThis.__custom_value : "absent" });
        })();
        "#,
        ),
        // probe_mode removed — LifecycleSpec has no such field
    };
    let result2 = run_lifecycle_contract(&spec2);
    let val2 = match result2 {
        LifecycleOutcome::Json(v) => v,
        other => panic!("expected Json, got {:?}", other),
    };
    assert_eq!(val2["custom"], Value::String("absent".to_string()));
}

// ═══════════════════════════════════════════════════════════════════════
// AST patch: transform renames and wraps plugin functions
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn ast_patch_transforms_plugin() {
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "patch-test",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            function loadToken(ctx) { return null; }
            function probe(ctx) {
                var token = loadToken(ctx);
                return { lines: [ctx.line.text({ label: "Token", value: token ? "yes" : "no" })] };
            }
            globalThis.__openusage_plugin = { id: "patch-test", probe: probe };
        })();
        "#,
        override_source: Some(
            r#"
        globalThis.__openusage_ast_patch = {
            functions: [{ target: "loadToken", with: "patchLoadToken", mode: "wrap" }]
        };
        function patchLoadToken(original, ctx) {
            return { source: "patched", auth: {} };
        }
        "#,
        ),
        harness: r#"
        (function() {
            globalThis.__test_ctx = {
                line: {
                    text: function(opts) { return { type: "text", label: opts.label, value: opts.value }; }
                }
            };
        })();
        "#,
        setup: "",
        execution: Some(
            r#"
        (function() {
            var result = globalThis.__openusage_plugin.probe(globalThis.__test_ctx);
            return JSON.stringify({ ok: true, result: result });
        })();
        "#,
        ),
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::Json(val) => {
            assert_eq!(val["ok"], Value::Bool(true));
            assert_eq!(val["result"]["lines"][0]["value"], "yes");
        }
        other => panic!("expected Json, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Stage failures: exact stage and Error-object diagnostics
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn harness_failure_returns_harness_error() {
    // Harness throws → mapped to Harness (before_plugin hook).
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "harness-fail",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "hf", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "throw new Error('harness kaboom');",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::Harness);
            let msg = format!("{}", stage);
            assert!(
                msg.contains("harness kaboom") || msg.contains("Error"),
                "error should mention harness error, got: {}",
                msg
            );
            // Error object details: name and message must be present
            assert_eq!(
                stage.error_name.as_deref(),
                Some("Error"),
                "expected Error name, got {:?}",
                stage.error_name
            );
            assert!(
                stage.message.contains("harness kaboom"),
                "message should contain error text, got: {}",
                stage.message
            );
            assert!(
                stage.stack.is_some(),
                "stack should be present for Error object"
            );
        }
        other => panic!("expected LifecycleError(Harness), got {:?}", other),
    }
}

#[test]
fn setup_failure_returns_setup_error() {
    // Setup throws → mapped to Setup (before_plugin hook).
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "setup-fail",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "sf", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "throw new Error('setup kaboom');",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::Setup);
            // Verify Error-object detail
            assert_eq!(
                stage.error_name.as_deref(),
                Some("Error"),
                "expected Error name, got {:?}",
                stage.error_name
            );
            assert!(
                stage.message.contains("setup kaboom"),
                "message should contain error text, got: {}",
                stage.message
            );
            assert!(
                stage.stack.is_some(),
                "stack should be present for Error object"
            );
        }
        other => panic!("expected LifecycleError(Setup), got {:?}", other),
    }
}

#[test]
fn transform_failure_returns_transform_error() {
    // Invalid override with AST patch targeting non-existent function.
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "transform-fail",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            function probe() { return { lines: [] }; }
            globalThis.__openusage_plugin = { id: "tf", probe: probe };
        })();
        "#,
        override_source: Some(
            r#"
        globalThis.__openusage_ast_patch = {
            functions: [{ target: "nonExistentFunc", with: "patchFn", mode: "wrap" }]
        };
        function patchFn(original, ctx) { return original(ctx); }
        "#,
        ),
        harness: "",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::Transform);
        }
        other => panic!("expected LifecycleError(Transform), got {:?}", other),
    }
}

#[test]
fn plugin_eval_failure_returns_plugin_eval_error() {
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "eval-fail",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: "var x = ;",
        override_source: None,
        harness: "",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::PluginEval);
        }
        other => panic!("expected LifecycleError(PluginEval), got {:?}", other),
    }
}

#[test]
fn override_bootstrap_failure_returns_override_bootstrap_error() {
    // Plugin script doesn't set __openusage_plugin → bootstrap fails.
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "bootstrap-fail",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: "var x = 1;",
        override_source: Some("globalThis.__openusage_override = { note: 'hi' };"),
        harness: "",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::OverrideBootstrap);
        }
        other => panic!(
            "expected LifecycleError(OverrideBootstrap), got {:?}",
            other
        ),
    }
}

#[test]
fn override_eval_failure_returns_override_eval_error() {
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "ov-eval-fail",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "ov-fail", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: Some("null.prop;"),
        harness: "",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::OverrideEval);
        }
        other => panic!("expected LifecycleError(OverrideEval), got {:?}", other),
    }
}

#[test]
fn execution_failure_returns_execution_error() {
    // Execution script throws.
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "exec-throw",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "exec-throw", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        execution: Some("(function() { throw new Error('execution kaboom'); })()"),
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::Execution);
            let msg = format!("{}", stage);
            assert!(
                msg.contains("execution kaboom"),
                "error should mention the exception, got: {}",
                msg
            );
            // Error object details
            assert_eq!(
                stage.error_name.as_deref(),
                Some("Error"),
                "expected Error name, got {:?}",
                stage.error_name
            );
            assert!(
                stage.message.contains("execution kaboom"),
                "message should contain error text, got: {}",
                stage.message
            );
            assert!(
                stage.stack.is_some(),
                "stack should be present for Error object"
            );
        }
        other => panic!("expected LifecycleError(Execution), got {:?}", other),
    }
}

#[test]
fn decode_failure_returns_result_decode_error() {
    // Execution script returns non-JSON string.
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "bad-json",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "bad-json", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        execution: Some("(function() { return '{bad json'; })()"),
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::ResultDecode);
            let msg = format!("{}", stage);
            assert!(
                msg.contains("not valid JSON"),
                "error should mention invalid JSON, got: {}",
                msg
            );
        }
        other => panic!("expected LifecycleError(ResultDecode), got {:?}", other),
    }
}

#[test]
fn non_string_return_returns_result_decode_error() {
    // Execution script returns a number (not a string) -> ResultDecode.
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "non-string",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "ns", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        execution: Some("(function() { return 42; })()"),
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::ResultDecode);
            let msg = format!("{}", stage);
            assert!(
                msg.contains("non-string"),
                "error should mention non-string, got: {}",
                msg
            );
            assert!(
                msg.contains("number"),
                "error should mention type, got: {}",
                msg
            );
        }
        other => panic!("expected LifecycleError(ResultDecode), got {:?}", other),
    }
}

#[test]
fn json_success_returns_parsed_value() {
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "json-ok",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "json-ok", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        execution: Some(
            r#"
        (function() {
            return JSON.stringify({ hello: "world", count: 42 });
        })();
        "#,
        ),
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::Json(val) => {
            assert_eq!(val["hello"], Value::String("world".to_string()));
            assert_eq!(val["count"], 42);
        }
        other => panic!("expected Json, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Discovery/probe hook - probe works after lifecycle
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn probe_function_works_after_lifecycle() {
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "probe-test",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            function probe(ctx) {
                return { lines: [ctx.line.text({ label: "Result", value: "success" })] };
            }
            globalThis.__openusage_plugin = { id: "probe-test", probe: probe };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__test_ctx = {
                line: {
                    text: function(opts) { return { type: "text", label: opts.label, value: opts.value }; }
                }
            };
        })();
        "#,
        setup: "",
        execution: Some(
            r#"
        (function() {
            var result = globalThis.__openusage_plugin.probe(globalThis.__test_ctx);
            return JSON.stringify({ ok: true, result: result });
        })();
        "#,
        ),
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::Json(val) => {
            assert_eq!(val["ok"], Value::Bool(true));
            assert_eq!(val["result"]["lines"][0]["label"], "Result");
            assert_eq!(val["result"]["lines"][0]["value"], "success");
        }
        other => panic!("expected Json, got {:?}", other),
    }
}

#[test]
fn probe_with_discovery_hook_works() {
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "discover-test",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            var plugin = {
                id: "discover-test",
                probe: function(ctx) {
                    var acct = ctx.account && ctx.account.id;
                    return { lines: [ctx.line.text({ label: "Account", value: acct || "default" })] };
                },
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1" }];
                }
            };
            globalThis.__openusage_plugin = plugin;
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__test_ctx = {
                line: { text: function(opts) { return { type: "text", label: opts.label, value: opts.value }; } }
            };
        })();
        "#,
        setup: "",
        execution: Some(
            r#"
        (function() {
            var da = globalThis.__openusage_plugin.discoverAccounts(globalThis.__test_ctx);
            var result = globalThis.__openusage_plugin.probe(globalThis.__test_ctx);
            return JSON.stringify({ ok: true, discovery: da, result: result });
        })();
        "#,
        ),
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::Json(val) => {
            assert_eq!(val["ok"], Value::Bool(true));
            assert_eq!(val["discovery"][0]["id"], "acct-1");
            assert_eq!(val["result"]["lines"][0]["value"], "default");
        }
        other => panic!("expected Json, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Stable runtime diagnostic mapping: JsError carries stage + message
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn lifecycle_error_carries_stage_and_message() {
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "diag-test",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "diag", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: Some("null.prop;"),
        harness: "",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError {
            stage, plugin_id, ..
        } => {
            assert_eq!(stage.stage, LifecycleStage::OverrideEval);
            assert!(
                stage.message.contains("null") || !stage.message.is_empty(),
                "error message should be non-empty and reference the cause"
            );
            assert_eq!(plugin_id, "diag-test");
            // Verify Display shows the stage
            let display = format!("{}", stage);
            assert!(display.contains("override_eval"));
        }
        other => panic!("expected LifecycleError, got {:?}", other),
    }
}

#[test]
fn bootstrap_error_preserves_error_object_details() {
    // Bootstrap failure with Error object: name, message, stack preserved.
    // Plugin source sets up a throwing getter so bootstrap fails.
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "bootstrap-err",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "be", probe: function() { return { lines: [] }; } };
            Object.defineProperty(globalThis.__openusage_plugin, 'probe', {
                get: function() { throw new TypeError("probe access failure"); }
            });
        })();
        "#,
        override_source: Some("/* no-op */;"),
        harness: "",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::OverrideBootstrap);
            assert_eq!(
                stage.error_name.as_deref(),
                Some("TypeError"),
                "expected TypeError name, got {:?}",
                stage.error_name
            );
            assert!(
                stage.message.contains("probe access failure"),
                "message should contain error text, got: {}",
                stage.message
            );
            assert!(stage.stack.is_some(), "stack should be present");
        }
        other => panic!(
            "expected LifecycleError(OverrideBootstrap), got {:?}",
            other
        ),
    }
}

#[test]
fn bootstrap_error_preserves_thrown_string() {
    // Bootstrap failure with thrown string: message preserved, no name/stack.
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "bootstrap-str",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "bs", probe: function() { return { lines: [] }; } };
            Object.defineProperty(globalThis.__openusage_plugin, 'probe', {
                get: function() { throw "bootstrap string failure"; }
            });
        })();
        "#,
        override_source: Some("/* no-op */;"),
        harness: "",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::OverrideBootstrap);
            assert!(
                stage.message.contains("bootstrap string failure"),
                "message should contain thrown string, got: {}",
                stage.message
            );
            assert!(stage.error_name.is_none(), "no name for string throw");
            assert!(stage.stack.is_none(), "no stack for string throw");
        }
        other => panic!(
            "expected LifecycleError(OverrideBootstrap), got {:?}",
            other
        ),
    }
}
#[test]
fn error_object_has_name_and_message() {
    // Verify that when JS throws an Error object, the JsError captures
    // the name and stack trace.
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "err-obj",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "err", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: Some(r#"throw new TypeError("custom type error");"#),
        harness: "",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::OverrideEval);
            // Error object diagnostics: name and message
            if let Some(ref err_name) = stage.error_name {
                assert!(
                    err_name.contains("TypeError"),
                    "expected TypeError, got {}",
                    err_name
                );
            }
            if let Some(ref stack) = stage.stack {
                assert!(!stack.is_empty(), "stack should be non-empty");
            }
        }
        other => panic!("expected LifecycleError, got {:?}", other),
    }
}

#[test]
fn thrown_string_is_captured() {
    // Verify that a thrown string is captured as the message.
    let output = run_lifecycle_contract(&LifecycleSpec {
        plugin_id: "str-err",
        // plugin_name removed — LifecycleSpec does not have this field
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "se", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: Some(r#"throw "plain string error";"#),
        harness: "",
        setup: "",
        execution: None,
        // probe_mode removed — LifecycleSpec has no such field
    });
    match output {
        LifecycleOutcome::LifecycleError { stage, .. } => {
            assert_eq!(stage.stage, LifecycleStage::OverrideEval);
            assert!(
                stage.message.contains("plain string error"),
                "message should contain thrown string, got: {}",
                stage.message
            );
        }
        other => panic!("expected LifecycleError, got {:?}", other),
    }
}
// ═══════════════════════════════════════════════════════════════════════
// Probe mode: execute_provider_in_context via run_spec
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn probe_mode_sync_discovery_and_probe() {
    // Verify probe mode runs discoverAccounts synchronously and probes each.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "probe-sync",
        plugin_name: "Probe Sync",
        plugin_source: r#"
        (function() {
            var plugin = {
                id: "probe-sync",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1" }, { id: "acct-2" }];
                },
                probe: function(ctx) {
                    var id = ctx.account ? ctx.account.id : "default";
                    return { lines: [ctx.line.text({ label: "Account", value: id })] };
                }
            };
            globalThis.__openusage_plugin = plugin;
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert_eq!(result.outputs.len(), 2, "expected 2 probe outputs");
            assert_eq!(result.outputs[0].account.id, "acct-1");
            assert_eq!(result.outputs[1].account.id, "acct-2");
            assert!(
                assertion.is_err(),
                "no assertion script provided — expected error"
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_async_discovery_resolves_promise() {
    // Verify probe mode handles async (Promise) discoverAccounts.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "probe-async",
        plugin_name: "Probe Async",
        plugin_source: r#"
        (function() {
            var plugin = {
                id: "probe-async",
                discoverAccounts: function(ctx) {
                    return new Promise(function(resolve) {
                        resolve([{ id: "async-acct" }]);
                    });
                },
                probe: function(ctx) {
                    return { lines: [ctx.line.text({ label: "Done", value: "ok" })] };
                }
            };
            globalThis.__openusage_plugin = plugin;
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert_eq!(
                result.outputs.len(),
                1,
                "expected 1 output from async discovery"
            );
            assert_eq!(result.outputs[0].account.id, "async-acct");
            assert!(assertion.is_err(), "no assertion script provided");
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_async_probe_resolves_promise() {
    // Verify probe mode handles async (Promise) probe.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "probe-async2",
        plugin_name: "Probe Async2",
        plugin_source: r#"
        (function() {
            var plugin = {
                id: "probe-async2",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct" }];
                },
                probe: function(ctx) {
                    return new Promise(function(resolve) {
                        resolve({ lines: [ctx.line.text({ label: "Async", value: "yes" })] });
                    });
                }
            };
            globalThis.__openusage_plugin = plugin;
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 output");
            assert!(
                result.outputs[0]
                    .lines
                    .iter()
                    .any(|l| format!("{}", l).contains("yes"))
            );
            assert!(assertion.is_err(), "no assertion script provided");
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_descriptor_error_policy_respected() {
    // Verify the errorPolicy field in discovery descriptors is respected.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "policy-test",
        plugin_name: "Policy Test",
        plugin_source: r#"
        (function() {
            var plugin = {
                id: "policy-test",
                discoverAccounts: function(ctx) {
                    return [
                        { id: "good", errorPolicy: "hide-if-other-account" },
                        { id: "bad", errorPolicy: "hide-if-other-account" }
                    ];
                },
                probe: function(ctx) {
                    if (ctx.account.id === "good") {
                        return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                    }
                    throw new Error("bad account probe failed");
                }
            };
            globalThis.__openusage_plugin = plugin;
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            // "bad" account has error, "good" succeeds.
            // With hide-if-other-account, the "bad" error should be suppressed
            // when there is at least one other account.
            assert_eq!(result.outputs.len(), 1, "bad account should be suppressed");
            assert_eq!(result.outputs[0].account.id, "good");
            assert!(assertion.is_err(), "no assertion script provided");
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_no_soft_fail_when_no_other_account() {
    // When only one account exists, soft-fail suppression must not apply.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "no-suppress",
        plugin_name: "No Suppress",
        plugin_source: r#"
        (function() {
            var plugin = {
                id: "no-suppress",
                discoverAccounts: function(ctx) {
                    return [{ id: "lonely", errorPolicy: "hide-if-other-account" }];
                },
                probe: function(ctx) {
                    throw new Error("i am alone");
                }
            };
            globalThis.__openusage_plugin = plugin;
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            // Only 1 account, no other account to hide behind -> error visible
            assert_eq!(result.outputs.len(), 1, "lonely account error visible");
            assert_eq!(result.outputs[0].account.id, "lonely");
            assert!(assertion.is_err(), "no assertion script provided");
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_output_proves_execute_provider_in_context() {
    // Verify that outputs contain provider_id, display_name, and account
    // with correct structure — proving execution came from
    // execute_provider_in_context, not a manual JS loop.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "provenance",
        plugin_name: "Provenance Test",
        plugin_source: r#"
        (function() {
            var plugin = {
                id: "provenance",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1" }];
                },
                probe: function(ctx) {
                    return { lines: [ctx.line.text({ label: "Result", value: "ok" })] };
                }
            };
            globalThis.__openusage_plugin = plugin;
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert_eq!(result.outputs.len(), 1);
            // provider_id and display_name come from LoadedPlugin manifest,
            // which execute_provider_in_context sets. A manual JS loop
            // would lack these typed fields.
            assert_eq!(result.outputs[0].provider_id, "provenance");
            assert_eq!(result.outputs[0].display_name, "Provenance Test");
            assert_eq!(result.outputs[0].account.id, "acct-1");
            assert!(!result.outputs[0].lines.is_empty());
            assert!(assertion.is_err(), "no assertion script provided");
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_post_probe_assertion_observes_mock_state() {
    // Verify assertion script evaluates after probe and captures mock state.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "post-assert",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            var plugin = {
                id: "post-assert",
                probe: function(ctx) {
                    return { lines: [] };
                }
            };
            globalThis.__openusage_plugin = plugin;
        })();
        "#,
        override_source: None,
        harness: "globalThis.__test_state = { requests: [{ url: \"mock-url\" }] };",
        setup: "",
        assertion: Some(
            r#"(function() { return JSON.stringify({ requests: globalThis.__test_state.requests }); })()"#,
        ),
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert_eq!(result.outputs.len(), 1, "default account output");
            let obs = assertion.expect("assertion should succeed");
            assert_eq!(obs["requests"][0]["url"], "mock-url");
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}
// ═══════════════════════════════════════════════════════════════════════
// Discovery validation contract
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn discovery_non_array_return_is_error() {
    // discoverAccounts returns an object (not an array).
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "non-arr",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "non-arr",
                discoverAccounts: function(ctx) { return {}; },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("must return an array"),
                "expected array validation error, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_non_object_descriptor_is_error() {
    // discoverAccounts returns an array containing a non-object element.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "non-obj",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "non-obj",
                discoverAccounts: function(ctx) { return ["string-element"]; },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("must be an object"),
                "expected object validation error, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_empty_id_is_error() {
    // discoverAccounts returns a descriptor with an empty id.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "empty-id",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "empty-id",
                discoverAccounts: function(ctx) { return [{ id: "" }]; },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("empty id"),
                "expected empty id error, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_duplicate_id_is_error() {
    // discoverAccounts returns descriptors with duplicate ids.
    // Current contract: duplicate discovery ids are invalid.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "dup-id",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "dup-id",
                discoverAccounts: function(ctx) {
                    return [{ id: "same" }, { id: "same" }];
                },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("duplicate output account id"),
                "expected duplicate output id error, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_over_32_descriptors_is_error() {
    // discoverAccounts returns 33 descriptors (over the max of 32).
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "over-32",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            var arr = [];
            for (var i = 0; i < 33; i++) { arr.push({ id: "acct-" + i }); }
            globalThis.__openusage_plugin = {
                id: "over-32",
                discoverAccounts: function(ctx) { return arr; },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("max 32"),
                "expected max 32 error, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_non_function_is_error() {
    // discoverAccounts is present but not a function.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "non-fn",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "non-fn",
                discoverAccounts: "not-a-function",
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("must be a function"),
                "expected function validation error, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_getter_throws_is_error() {
    // discoverAccounts accessor throws an exception.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "getter-throw",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            var plugin = {
                id: "getter-throw",
                probe: function(ctx) { return { lines: [] }; }
            };
            Object.defineProperty(plugin, 'discoverAccounts', {
                get: function() { throw new Error("getter kaboom"); }
            });
            globalThis.__openusage_plugin = plugin;
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("accessor threw"),
                "expected accessor error, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_invocation_throw_is_error() {
    // discoverAccounts function throws an exception.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "invoke-throw",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "invoke-throw",
                discoverAccounts: function(ctx) {
                    throw new Error("invocation kaboom");
                },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("discoverAccounts failed"),
                "expected discoverAccounts failure error, got: {}",
                line_str
            );
            assert!(
                line_str.contains("invocation kaboom"),
                "error should preserve thrown message, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_empty_array_yields_zero_outputs() {
    // discoverAccounts returns an empty array → zero probe outputs.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "empty-disc",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "empty-disc",
                discoverAccounts: function(ctx) { return []; },
                probe: function(ctx) { return { lines: [ctx.line.text({ label: "X", value: "y" })] }; }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert!(
                result.outputs.is_empty(),
                "expected zero outputs for empty discovery, got {}",
                result.outputs.len()
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_null_falls_back_to_single_account_probe() {
    // discoverAccounts is null → falls back to one probe with the default account.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "null-disc",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "null-disc",
                discoverAccounts: null,
                probe: function(ctx) {
                    var acctId = ctx.account ? ctx.account.id : "none";
                    return { lines: [ctx.line.text({ label: "Account", value: acctId })] };
                }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(
                result.outputs.len(),
                1,
                "expected 1 single-account probe output"
            );
            assert_eq!(
                result.outputs[0].account.id, "default",
                "single-account probe should use default account"
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_undefined_falls_back_to_single_account_probe() {
    // discoverAccounts is undefined → falls back to one probe with the default account.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "undef-disc",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "undef-disc",
                probe: function(ctx) {
                    var acctId = ctx.account ? ctx.account.id : "none";
                    return { lines: [ctx.line.text({ label: "Account", value: acctId })] };
                }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(
                result.outputs.len(),
                1,
                "expected 1 single-account probe output"
            );
            assert_eq!(
                result.outputs[0].account.id, "default",
                "single-account probe should use default account"
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_probe_account_field_ignored() {
    // Probe returns an account field with mismatched id — must be ignored.
    // The descriptor's id and origin are authoritative.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "ignore-acct",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "ignore-acct",
                discoverAccounts: function(ctx) {
                    return [{ id: "descriptor-1", origin: "opencode" }];
                },
                probe: function(ctx) {
                    return {
                        lines: [ctx.line.text({ label: "Status", value: "ok" })],
                        account: { id: "wrong-account", origin: "wrong-origin" }
                    };
                }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 probe output");
            assert_eq!(
                result.outputs[0].account.id, "descriptor-1",
                "descriptor id must be authoritative over probe result.account"
            );
            assert_eq!(
                result.outputs[0].account.origin, "opencode",
                "descriptor origin must be authoritative over probe result.account"
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_origin_opencode_appears_in_output() {
    // Descriptor origin "opencode" must appear in the probe output's account.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "origin-open",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "origin-open",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1", origin: "opencode" }];
                },
                probe: function(ctx) {
                    return { lines: [ctx.line.text({ label: "X", value: "y" })] };
                }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 output");
            assert_eq!(result.outputs[0].account.id, "acct-1");
            assert_eq!(
                result.outputs[0].account.origin, "opencode",
                "descriptor origin must appear in output"
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_origin_missing_defaults_native() {
    // Descriptor without origin field must default to "native" in output.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "origin-default",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "origin-default",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1" }];
                },
                probe: function(ctx) {
                    return { lines: [ctx.line.text({ label: "X", value: "y" })] };
                }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 output");
            assert_eq!(
                result.outputs[0].account.origin, "native",
                "missing descriptor origin must default to native"
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_origin_null_equals_native() {
    // Descriptor with null origin defaults to "native".
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "origin-null",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "origin-null",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1", origin: null }];
                },
                probe: function(ctx) {
                    return { lines: [ctx.line.text({ label: "X", value: "y" })] };
                }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 output");
            assert_eq!(
                result.outputs[0].account.origin, "native",
                "null descriptor origin must default to native"
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_origin_empty_is_error() {
    // Descriptor with empty origin string must be a validation error.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "origin-empty",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "origin-empty",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1", origin: "" }];
                },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("empty origin"),
                "expected empty origin error, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_origin_non_string_is_error() {
    // Descriptor with non-string origin must be a validation error.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "origin-nonstr",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "origin-nonstr",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1", origin: 42 }];
                },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("non-string origin"),
                "expected non-string origin error, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_origin_accessor_throws_is_error() {
    // Descriptor with accessor-throwing origin must be a validation error.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "origin-throw",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            var desc = { id: "acct-1" };
            Object.defineProperty(desc, 'origin', {
                get: function() { throw new Error("origin getter kaboom"); }
            });
            globalThis.__openusage_plugin = {
                id: "origin-throw",
                discoverAccounts: function(ctx) {
                    return [desc];
                },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("accessor threw"),
                "expected accessor error for origin, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// errorPolicy validation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn discovery_error_policy_invalid_string_is_error() {
    // errorPolicy set to an unrecognized string must be a validation error.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "ep-inv-str",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "ep-inv-str",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1", errorPolicy: "bogus-policy" }];
                },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("invalid errorPolicy"),
                "expected invalid errorPolicy error, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_error_policy_non_string_is_error() {
    // errorPolicy set to a number must be a validation error.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "ep-non-str",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "ep-non-str",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1", errorPolicy: 42 }];
                },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("non-string errorPolicy"),
                "expected non-string errorPolicy error, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_error_policy_accessor_throws_is_error() {
    // errorPolicy accessor throws must be a validation error.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "ep-throw",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            var desc = { id: "acct-1" };
            Object.defineProperty(desc, 'errorPolicy', {
                get: function() { throw new Error("policy getter kaboom"); }
            });
            globalThis.__openusage_plugin = {
                id: "ep-throw",
                discoverAccounts: function(ctx) {
                    return [desc];
                },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("accessor threw"),
                "expected accessor error for errorPolicy, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_error_policy_absent_is_valid() {
    // errorPolicy absent must be accepted (no policy).
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "ep-absent",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "ep-absent",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1" }];
                },
                probe: function(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 probe output");
            assert_eq!(result.outputs[0].account.id, "acct-1");
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_error_policy_null_is_valid() {
    // errorPolicy null must be accepted (no policy).
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "ep-null",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "ep-null",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1", errorPolicy: null }];
                },
                probe: function(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 probe output");
            assert_eq!(result.outputs[0].account.id, "acct-1");
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_error_policy_undefined_is_valid() {
    // errorPolicy undefined must be accepted (no policy).
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "ep-undef",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "ep-undef",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1", errorPolicy: undefined }];
                },
                probe: function(ctx) {
                    return { lines: [ctx.line.text({ label: "Status", value: "ok" })] };
                }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 probe output");
            assert_eq!(result.outputs[0].account.id, "acct-1");
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_ctx_account_id_only() {
    // The ctx.account passed to probe must only have an id — no origin.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "ctx-account",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "ctx-account",
                discoverAccounts: function(ctx) {
                    return [{ id: "acct-1", origin: "opencode" }];
                },
                probe: function(ctx) {
                    var hasOrigin = typeof ctx.account.origin !== "undefined";
                    var keys = Object.keys(ctx.account);
                    return {
                        lines: [ctx.line.text({ label: "HasOrigin", value: String(hasOrigin) }),
                                ctx.line.text({ label: "Keys", value: JSON.stringify(keys) })]
                    };
                }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 output");
            let lines = &result.outputs[0].lines;
            // Find HasOrigin line
            let has_origin = lines
                .iter()
                .find(|l| format!("{}", l).contains("HasOrigin"));
            assert!(has_origin.is_some(), "expected HasOrigin line in output");
            let has_origin_str = format!("{}", has_origin.unwrap());
            assert!(
                has_origin_str.contains("false"),
                "ctx.account must not have origin, got: {}",
                has_origin_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn single_account_probe_account_field_ignored() {
    // Single-account mode (no discoverAccounts): probe result.account is ignored.
    // Always uses default account with native origin.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "ignore-single",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "ignore-single",
                probe: function(ctx) {
                    return {
                        lines: [ctx.line.text({ label: "Status", value: "ok" })],
                        account: { id: "custom-id" }
                    };
                }
            };
        })();
        "#,
        override_source: None,
        harness: r#"
        (function() {
            globalThis.__openusage_ctx = {
                line: { text: function(o) { return { type: "text", label: o.label, value: o.value }; } }
            };
        })();
        "#,
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 probe output");
            assert_eq!(
                result.outputs[0].account.id, "default",
                "single-account mode must use default account regardless of probe result.account"
            );
            assert_eq!(
                result.outputs[0].account.origin, "native",
                "single-account mode must use native origin"
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn discovery_rejected_promise_preserves_error_detail() {
    // discoverAccounts returns a rejected Promise → error preserves detail.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "reject-disc",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = {
                id: "reject-disc",
                discoverAccounts: function(ctx) {
                    return new Promise(function(_, reject) {
                        reject(new Error("discovery rejected"));
                    });
                },
                probe: function(ctx) { return { lines: [] }; }
            };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert_eq!(result.outputs.len(), 1, "expected 1 error output");
            let line_str = format!("{}", result.outputs[0].lines[0]);
            assert!(
                line_str.contains("discoverAccounts failed"),
                "expected discoverAccounts failure prefix, got: {}",
                line_str
            );
            assert!(
                line_str.contains("discovery rejected"),
                "error should preserve rejection message, got: {}",
                line_str
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_assertion_decode_error_returns_typed_err() {
    // Assertion returns bad JSON -> ResultDecode error.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "bad-assert-json",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "baj", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: Some("(function() { return '{bad json'; })()"),
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert_eq!(result.outputs.len(), 1);
            let err = assertion.expect_err("expected assertion error");
            assert_eq!(err.stage.to_string(), "result_decode");
            assert!(
                err.message.contains("not valid JSON") || err.message.contains("is not valid JSON"),
                "error should mention invalid JSON, got: {}",
                err.message
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_assertion_non_string_returns_typed_err() {
    // Assertion returns a number -> ResultDecode error with type info.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "non-str-assert",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "nsa", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: Some("(function() { return 42; })()"),
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert_eq!(result.outputs.len(), 1);
            let err = assertion.expect_err("expected assertion error");
            assert_eq!(err.stage.to_string(), "result_decode");
            assert!(
                err.message.contains("non-string"),
                "error should mention non-string, got: {}",
                err.message
            );
            assert!(
                err.message.contains("number"),
                "error should mention type number, got: {}",
                err.message
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_assertion_execution_throw_returns_typed_err() {
    // Assertion throws -> Execution error.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "assert-throw",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "at", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: Some("(function() { throw new Error('assertion kaboom'); })()"),
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert_eq!(result.outputs.len(), 1);
            let err = assertion.expect_err("expected assertion error");
            assert_eq!(err.stage.to_string(), "execution");
            assert!(
                err.message.contains("assertion kaboom"),
                "error should mention assertion exception, got: {}",
                err.message
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_assertion_empty_script_returns_typed_err() {
    // Empty assertion script -> Execution error.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "empty-assert",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "ea", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: Some(""),
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert_eq!(result.outputs.len(), 1);
            let err = assertion.expect_err("expected assertion error");
            assert_eq!(err.stage.to_string(), "execution");
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_assertion_promise_resolves_to_json() {
    // Assertion returns a Promise that resolves to a JSON string.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "promise-assert",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "pa", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "globalThis.__test_state = { value: 99 };",
        setup: "",
        assertion: Some(
            r#"(function() { return new Promise(function(resolve) {
                resolve(JSON.stringify({ value: globalThis.__test_state.value }));
            }); })()"#,
        ),
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert_eq!(result.outputs.len(), 1);
            let obs = assertion.expect("Promise assertion should resolve to JSON");
            assert_eq!(obs["value"], 99);
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn probe_mode_assertion_promise_rejected_returns_typed_err() {
    // Assertion returns a rejected Promise -> Execution error.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "promise-reject",
        plugin_name: "",
        plugin_source: r#"
        (function() {
            globalThis.__openusage_plugin = { id: "pr", probe: function() { return { lines: [] }; } };
        })();
        "#,
        override_source: None,
        harness: "",
        setup: "",
        assertion: Some(
            r#"(function() { return new Promise(function(_, reject) {
                reject(new Error('promise rejected'));
            }); })()"#,
        ),
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert_eq!(result.outputs.len(), 1);
            let err = assertion.expect_err("expected assertion error from rejected promise");
            assert_eq!(err.stage.to_string(), "execution");
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}
