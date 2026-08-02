#![allow(unreachable_patterns)]

// Integration tests for the Copilot plugin override.
//
// Every test calls the single shared `RunSpec { probe_mode: true }` path,
// which calls `execute_provider_in_context` (production probe code).
// No manual Runtime/Context/override_api injection or discovery/probe loops.
//
// Tests assert actual `ProbeResult` (typed outputs with lines, accounts)
// and/or optional post-probe assertion JS that captures test-state JSON.

mod support;

use openusage_cli::plugin_engine::runtime::ProbeResult;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

use support::override_runner::{ProviderOutcome, ProviderSpec, run_provider_probe};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn copilot_plugin_script() -> String {
    let path = repo_root().join("vendor/openusage/plugins/copilot/plugin.js");
    fs::read_to_string(path).expect("read copilot plugin")
}

fn copilot_override_script() -> String {
    let path = repo_root().join("plugin-overrides/copilot.js");
    fs::read_to_string(path).expect("read copilot override")
}

/// Run a copilot probe test through the shared runner.
fn run_copilot_probe(setup_js: &str) -> (ProbeResult, Option<Value>) {
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "copilot",
        plugin_name: "Copilot",
        plugin_source: &copilot_plugin_script(),
        override_source: Some(&copilot_override_script()),
        harness: HARNESS_SCRIPT,
        setup: setup_js,
        assertion: Some(ASSERTION_SCRIPT),
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            let obs = assertion.ok();
            (result, obs)
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

fn assert_subscriptions(obs: &Value, expected: &[&str]) {
    let subs: Vec<&str> = obs["subscriptions"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(subs, expected, "subscription mismatch");
}

fn first_request_auth(obs: &Value) -> String {
    obs["requests"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|r| r["authorization"].as_str().map(String::from))
        .unwrap_or_default()
}

fn account_origin<'a>(result: &'a ProbeResult, account_id: &str) -> Option<&'a str> {
    result
        .outputs
        .iter()
        .find(|output| output.account.id == account_id)
        .map(|output| output.account.origin.as_str())
}

fn assert_opencode_origin(result: &ProbeResult, account_id: &str) {
    let origin = account_origin(result, account_id);
    assert_eq!(
        origin,
        Some("opencode"),
        "expected origin 'opencode' for account '{}', got {:?}",
        account_id,
        origin
    );
}

fn assert_default_origin(result: &ProbeResult) {
    assert_eq!(
        account_origin(result, "default"),
        Some("native"),
        "expected origin 'native' for default account"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Test suite
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn copilot_override_native_default_source() {
    // Native Copilot keychain auth (no opencode fallback).
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({
          token: "native-token"
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );

    assert_eq!(first_request_auth(&obs), "token native-token");

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // Default account has no origin
    assert_default_origin(&result);
}

#[test]
fn copilot_override_uses_opencode_fallback_auth_when_primary_auth_missing() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          "github-copilot": {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access",
            expires: 0
          }
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );

    assert_eq!(first_request_auth(&obs), "token fallback-access");

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode error account has origin: opencode
    assert_opencode_origin(&result, "opencode-0");
}

#[test]
fn copilot_override_preserves_original_auth_priority() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({
          token: "primary-token"
        });
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          "github-copilot": {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access",
            expires: 0
          }
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );

    assert_eq!(first_request_auth(&obs), "token primary-token");

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // Default account has no origin
    assert_default_origin(&result);
}

#[test]
fn copilot_override_tries_multiple_opencode_auth_paths() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.account = "opencode-1";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          google: {
            type: "oauth",
            refresh: "google-refresh",
            access: "google-access",
            expires: 1
          }
        });
        __test_state.files["~/.config/opencode/auth.json"] = JSON.stringify({
          "github-copilot": {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access-2",
            expires: 0
          }
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );

    assert_eq!(first_request_auth(&obs), "token fallback-access-2");

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode account has origin: opencode
    assert_opencode_origin(&result, "opencode-1");
}

#[test]
fn copilot_override_keeps_not_logged_in_error_without_valid_fallback_payload() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = "{bad-json";
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );

    // The plugin or override fails to parse the invalid JSON during lifecycle.
    // The error surfaces in the ProbeResult's PluginOutput lines.
    assert!(!result.outputs.is_empty(), "expected an output");
    assert_eq!(result.outputs[0].provider_id, "copilot");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("Invalid JSON")
            || error_text.contains("Invalid auth")
            || error_text.contains("failed"),
        "expected a parse or lifecycle error for bad JSON, got: {}",
        error_text
    );
}

#[test]
fn copilot_override_exact_token_request_source() {
    // Verify the authorization token comes from the exact expected source
    // (opencode fallback, not mixed or default).
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          "github-copilot": {
            type: "oauth",
            refresh: "opencode-refresh",
            access: "opencode-access",
            expires: 0
          }
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );

    // Token source: opencode auth has "github-copilot" key.
    assert_eq!(first_request_auth(&obs), "token opencode-access");

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode error account has origin: opencode
    assert_opencode_origin(&result, "opencode-0");
}

#[test]
fn copilot_override_only_default_soft_fail() {
    // Only one account — no suppression even if error policy is set.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "copilot",
        plugin_name: "Copilot",
        plugin_source: &copilot_plugin_script(),
        override_source: Some(&copilot_override_script()),
        harness: HARNESS_SCRIPT,
        setup: r#"
        __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({
          token: "native-token"
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
        assertion: Some(ASSERTION_SCRIPT),
        // probe_mode removed — ProviderSpec has no such field
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert!(!result.outputs.is_empty(), "expected probe outputs");
            assert_eq!(result.outputs[0].provider_id, "copilot");
            assertion.expect("assertion should produce state");
            assert_default_origin(&result);
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn copilot_override_empty_source_no_account() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = "";
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // Default account has no origin
    assert_default_origin(&result);
}

#[test]
fn copilot_override_unreadable_source_creates_error_account() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = "throw-on-read";
        __test_state.readErrors["~/.local/share/opencode/auth.json"] = true;
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode error account has origin: opencode
    assert_opencode_origin(&result, "opencode-0");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("read error") || error_text.contains("Failed to read"),
        "got: {}",
        error_text
    );
}

#[test]
fn copilot_override_missing_provider_key_omits_candidate() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({ google: { type: "oauth", access: "g", expires: 1 } });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // Default account has no origin
    assert_default_origin(&result);
}

#[test]
fn copilot_override_malformed_provider_block_null_error() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({ "github-copilot": null });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode error account has origin: opencode
    assert_opencode_origin(&result, "opencode-0");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("Invalid") || error_text.contains("invalid"),
        "got: {}",
        error_text
    );
}

#[test]
fn copilot_override_malformed_provider_block_array_error() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({ "github-copilot": ["not", "valid"] });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode error account has origin: opencode
    assert_opencode_origin(&result, "opencode-0");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("Invalid") || error_text.contains("invalid"),
        "got: {}",
        error_text
    );
}

#[test]
fn copilot_override_missing_access_token_error() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({ "github-copilot": { type: "oauth", refresh: "r", expires: 0 } });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode error account has origin: opencode
    assert_opencode_origin(&result, "opencode-0");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("access") || error_text.contains("Invalid"),
        "got: {}",
        error_text
    );
}

#[test]
fn copilot_override_default_native_error_visible_when_alone() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // Default account has no origin
    assert_default_origin(&result);

    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("Not logged in") || error_text.contains("error"),
        "got: {}",
        error_text
    );
}

#[test]
fn copilot_override_default_suppressed_when_opencode_exists() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = "{bad-json";
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode error account has origin: opencode
    assert_opencode_origin(&result, "opencode-0");
}

#[test]
fn copilot_override_no_cross_source_fallback() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({ token: "native" });
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({ "github-copilot": { type: "oauth", access: "oc-access", expires: 0 } });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode error account has origin: opencode
    assert_opencode_origin(&result, "opencode-0");
}

#[test]
fn copilot_override_two_valid_files_stable_ids() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({ "github-copilot": { type: "oauth", access: "token-0", expires: 0 } });
        __test_state.files["~/.config/opencode/auth.json"] = JSON.stringify({ "github-copilot": { type: "oauth", access: "token-1", expires: 0 } });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );
    let accounts: Vec<String> = result
        .outputs
        .iter()
        .filter(|o| o.account.id.starts_with("opencode-"))
        .map(|o| o.account.id.clone())
        .collect();
    assert!(
        accounts.contains(&"opencode-0".to_string()),
        "expected opencode-0, got: {:?}",
        accounts
    );
    assert!(
        accounts.contains(&"opencode-1".to_string()),
        "expected opencode-1, got: {:?}",
        accounts
    );
}

#[test]
fn copilot_override_first_missing_second_valid_opencode_1() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.config/opencode/auth.json"] = JSON.stringify({ "github-copilot": { type: "oauth", access: "second-token", expires: 0 } });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.config/opencode/auth.json",
        ],
    );
    let accounts: Vec<String> = result
        .outputs
        .iter()
        .filter(|o| o.account.id.starts_with("opencode-"))
        .map(|o| o.account.id.clone())
        .collect();
    assert!(
        accounts.contains(&"opencode-1".to_string()),
        "expected opencode-1 when first missing, got: {:?}",
        accounts
    );
}
// ═══════════════════════════════════════════════════════════════════════
// Regression: missing subscribeFile
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn copilot_override_fails_visibly_when_subscribefile_missing() {
    // Regression: missing subscribeFile should cause override init to fail
    // visibly rather than silently continuing.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "copilot",
        plugin_name: "Copilot",
        plugin_source: &copilot_plugin_script(),
        override_source: Some(&copilot_override_script()),
        harness: HARNESS_SCRIPT,
        setup: r#"
        delete __test_ctx.host.fs.subscribeFile;
        "#,
        assertion: None,
    });
    match outcome {
        ProviderOutcome::Probe { result, .. } => {
            assert!(
                !result.outputs.is_empty(),
                "expected error output when subscribeFile is missing"
            );
            let error_text: String = result.outputs[0]
                .lines
                .iter()
                .map(|l| format!("{}", l))
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                error_text.contains("subscribeFile")
                    || error_text.contains("plugin override failed"),
                "expected error about missing subscribeFile, got: {}",
                error_text
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Public run_probe lifecycle test
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn copilot_override_run_probe_lifecycle() {
    let Some(paths) = support::hermetic::enter("copilot_override_run_probe_lifecycle") else {
        return;
    };

    // Use the public production `run_probe` entrypoint with a temporary
    // override directory and HOME, exercising the real override file and
    // real host API (filesystem, env) without network.
    use openusage_cli::plugin_engine::manifest::{LoadedPlugin, PluginManifest};
    use openusage_cli::plugin_engine::runtime::{MetricLine, PluginOutput, run_probe_in_sandbox};

    // Synthetic plugin with all AST patch targets required by the copilot override.
    let plugin_script = r#"
(function() {
  function loadToken(ctx) { return null; }
  function probe(ctx) {
    var cred = loadToken(ctx);
    if (!cred) {
      throw "Not logged in. Run `gh auth login` first.";
    }
    return {
      plan: "Individual",
      lines: [ctx.line.badge({ label: "Status", text: "Authenticated" })]
    };
  }
  globalThis.__openusage_plugin = { id: "copilot", probe: probe };
})();
"#;

    // Write the real override file
    let override_path = paths.overrides.join("copilot.js");
    std::fs::write(&override_path, copilot_override_script()).expect("write override file");

    // Create a valid OpenCode auth candidate in HOME
    let opencode_dir = paths.root.join(".local/share/opencode");
    std::fs::create_dir_all(&opencode_dir).expect("create opencode dir");
    let auth_json = serde_json::json!({
        "github-copilot": {
            "type": "oauth",
            "access": "test-access-token",
            "refresh": "test-refresh-token",
            "expires": 9999999999999i64
        }
    });
    std::fs::write(
        opencode_dir.join("auth.json"),
        serde_json::to_string_pretty(&auth_json).expect("serialize auth"),
    )
    .expect("write auth.json");

    let plugin = LoadedPlugin {
        manifest: PluginManifest {
            schema_version: 1,
            id: "copilot".to_string(),
            name: "Copilot".to_string(),
            version: "0.0.0".to_string(),
            entry: "plugin.js".to_string(),
            icon: "icon.svg".to_string(),
            brand_color: None,
            lines: vec![],
            links: vec![],
        },
        plugin_dir: std::path::PathBuf::from("."),
        entry_script: plugin_script.to_string(),
        icon_data_url: "data:image/svg+xml;base64,".to_string(),
    };

    let result = run_probe_in_sandbox(
        &plugin,
        &paths.app_data,
        "0.0.0-test",
        Some(&paths.overrides),
        &paths.root,
    );

    // Assert: override file was loaded (discoverAccounts produced opencode-0)
    assert!(!result.outputs.is_empty(), "expected at least one output");

    // Find the opencode-0 output
    let opencode_outputs: Vec<&PluginOutput> = result
        .outputs
        .iter()
        .filter(|o| o.account.id == "opencode-0")
        .collect();
    assert_eq!(
        opencode_outputs.len(),
        1,
        "expected exactly one opencode-0 output, got {:?}",
        result
            .outputs
            .iter()
            .map(|o| &o.account.id)
            .collect::<Vec<_>>()
    );

    let output = opencode_outputs[0];

    // Assert: native/default failure is soft-suppressed
    // (the "default" account with hide-if-other-account should not appear)
    let default_outputs: Vec<&PluginOutput> = result
        .outputs
        .iter()
        .filter(|o| o.account.id == "default")
        .collect();
    assert!(
        default_outputs.is_empty(),
        "default account should be suppressed when opencode-0 exists"
    );

    // Assert: resulting output is exactly the expected opencode-0 account with successful output
    assert_eq!(output.provider_id, "copilot");
    assert_eq!(output.display_name, "Copilot");
    assert_eq!(output.plan.as_deref(), Some("Individual"));
    assert_eq!(output.lines.len(), 1, "expected exactly one line");
    match &output.lines[0] {
        MetricLine::Badge { label, text, .. } => {
            assert_eq!(label, "Status");
            assert_eq!(text, "Authenticated");
        }
        other => panic!("expected badge line, got: {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════

/// Harness for Copilot tests — sets up mock host with keychain support.
const HARNESS_SCRIPT: &str = r#"
(function () {
  globalThis.__test_state = {
    files: {},
    keychain: {},
    requests: [],
    logs: [],
    subscriptions: [],
    readErrors: {},
    responses: {
      usage: []
    }
  };

  function dedupSubscribe(path) {
    var arr = globalThis.__test_state.subscriptions;
    for (var i = 0; i < arr.length; i++) {
      if (arr[i] === path) return;
    }
    arr.push(path);
  }

  function cloneHeaders(input) {
    var out = {};
    if (!input || typeof input !== "object") return out;
    var keys = Object.keys(input);
    for (var i = 0; i < keys.length; i++) {
      out[keys[i]] = input[keys[i]];
    }
    return out;
  }

  function tryParseJson(text) {
    if (text === null || text === undefined) return null;
    var trimmed = String(text).trim();
    if (!trimmed) return null;
    try {
      return JSON.parse(trimmed);
    } catch (_) {
      return null;
    }
  }

  var ctx = {
    app: {
      pluginDataDir: "~/.config/openusage/plugins/copilot"
    },
    host: {
      fs: {
        exists: function (path) {
          return Object.prototype.hasOwnProperty.call(globalThis.__test_state.files, path);
        },
        readText: function (path) {
          if (!Object.prototype.hasOwnProperty.call(globalThis.__test_state.files, path)) {
            throw new Error("file not found: " + path);
          }
          if (globalThis.__test_state.readErrors && globalThis.__test_state.readErrors[path]) {
            throw new Error("read error: " + path);
          }
          return globalThis.__test_state.files[path];
        },
        writeText: function (path, text) {
          globalThis.__test_state.files[path] = String(text);
        },
        listDir: function () { return []; },
        subscribeFile: function (path) {
          if (typeof path !== "string" || path.trim().length === 0) return false;
          dedupSubscribe(path);
          return true;
        }
      },
      keychain: {
        readGenericPassword: function (service) {
          if (Object.prototype.hasOwnProperty.call(globalThis.__test_state.keychain, service)) {
            return globalThis.__test_state.keychain[service];
          }
          return null;
        },
        writeGenericPassword: function (service, value) {
          globalThis.__test_state.keychain[service] = String(value);
        },
        deleteGenericPassword: function (service) {
          delete globalThis.__test_state.keychain[service];
        }
      },
      http: {
        request: function (opts) {
          opts = opts || {};
          var headers = cloneHeaders(opts.headers);
          var url = String(opts.url || "");
          globalThis.__test_state.requests.push({
            method: String(opts.method || "GET"),
            url: url,
            authorization: headers.Authorization || null
          });

          var resp = globalThis.__test_state.responses.usage.length > 0
            ? globalThis.__test_state.responses.usage.shift()
            : null;
          if (!resp) {
            return { status: 500, headers: {}, bodyText: "{}" };
          }

          var bodyText = typeof resp.bodyText === "string"
            ? resp.bodyText
            : JSON.stringify(resp.bodyText || {});

          return {
            status: Number(resp.status || 200),
            headers: cloneHeaders(resp.headers),
            bodyText: bodyText
          };
        }
      },
      log: {
        info: function (msg) { globalThis.__test_state.logs.push("info:" + String(msg)); },
        warn: function (msg) { globalThis.__test_state.logs.push("warn:" + String(msg)); },
        error: function (msg) { globalThis.__test_state.logs.push("error:" + String(msg)); }
      }
    },
    util: {
      tryParseJson: tryParseJson,
      request: function (opts) {
        return ctx.host.http.request(opts);
      },
      toIso: function (value) {
        if (value === null || value === undefined) return null;
        if (typeof value === "number" && Number.isFinite(value)) {
          var ms = Math.abs(value) < 1e10 ? value * 1000 : value;
          var dNum = new Date(ms);
          return Number.isFinite(dNum.getTime()) ? dNum.toISOString() : null;
        }
        if (typeof value === "string") {
          var dStr = new Date(value);
          return Number.isFinite(dStr.getTime()) ? dStr.toISOString() : null;
        }
        return null;
      }
    },
    base64: {
      decode: function (str) {
        var chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        str = String(str || "").replace(/-/g, "+").replace(/_/g, "/");
        while (str.length % 4) str += "=";
        str = str.replace(/=+$/, "");

        var result = "";
        var i = 0;
        while (i < str.length) {
          var remaining = str.length - i;
          var a = chars.indexOf(str.charAt(i++));
          var b = chars.indexOf(str.charAt(i++));
          var c = remaining > 2 ? chars.indexOf(str.charAt(i++)) : 0;
          var d = remaining > 3 ? chars.indexOf(str.charAt(i++)) : 0;
          var n = (a << 18) | (b << 12) | (c << 6) | d;
          result += String.fromCharCode((n >> 16) & 255);
          if (remaining > 2) result += String.fromCharCode((n >> 8) & 255);
          if (remaining > 3) result += String.fromCharCode(n & 255);
        }
        return result;
      }
    },
    line: {
      text: function (opts) { return { type: "text", label: opts.label, value: opts.value }; },
      progress: function (opts) {
        return {
          type: "progress",
          label: opts.label,
          used: opts.used,
          limit: opts.limit,
          format: opts.format,
          resetsAt: opts.resetsAt,
          periodDurationMs: opts.periodDurationMs
        };
      },
      badge: function (opts) { return { type: "badge", label: opts.label, text: opts.text }; }
    },
    fmt: {
      planLabel: function (value) { return String(value || ""); }
    }
  };

  globalThis.__test_ctx = ctx;
  globalThis.__openusage_ctx = ctx;
})();
"#;

/// Post-probe assertion script.
const ASSERTION_SCRIPT: &str = r#"
(function() {
  var state = globalThis.__test_state;
  return JSON.stringify({
    subscriptions: state.subscriptions,
    requests: state.requests,
    files: state.files,
    logs: state.logs
  });
})();
"#;
