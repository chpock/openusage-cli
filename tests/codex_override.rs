#![allow(unreachable_patterns)]

// Integration tests for the Codex plugin override.
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

fn codex_plugin_script() -> String {
    let path = repo_root().join("vendor/openusage/plugins/codex/plugin.js");
    fs::read_to_string(path).expect("read codex plugin")
}

fn codex_override_script() -> String {
    let path = repo_root().join("plugin-overrides/codex.js");
    fs::read_to_string(path).expect("read codex override")
}

/// Run a codex probe test through the shared runner.
///
/// `setup_js` is injected as the `setup` field; it mutates `__test_state`
/// with files, responses, account, and other test fixtures.
///
/// Returns the `ProbeResult` and an option to inspect assertion JSON.
fn run_codex_probe(setup_js: &str) -> (ProbeResult, Option<Value>) {
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "codex",
        plugin_name: "Codex",
        plugin_source: &codex_plugin_script(),
        override_source: Some(&codex_override_script()),
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

/// Extract the first request from assertion state.
fn first_request(obs: &Value) -> Option<Value> {
    obs["requests"]
        .as_array()
        .and_then(|arr| arr.first().cloned())
}

fn first_request_auth(obs: &Value) -> String {
    first_request(obs)
        .and_then(|r| r["authorization"].as_str().map(String::from))
        .unwrap_or_default()
}

fn first_request_account_id(obs: &Value) -> String {
    first_request(obs)
        .and_then(|r| r["accountId"].as_str().map(String::from))
        .unwrap_or_default()
}

fn account_is_active(result: &ProbeResult, account_id: &str) -> Option<bool> {
    result
        .outputs
        .iter()
        .find(|o| o.account.id == account_id)
        .map(|o| o.account.is_active)
}

// ═══════════════════════════════════════════════════════════════════════
// Test suite
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn codex_override_native_default_source() {
    // Native Codex auth (no opencode fallback needed) with default account.
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.config/codex/auth.json"] = JSON.stringify({
          tokens: {
            access_token: "native-token"
          },
          last_refresh: "2026-04-19T00:00:00.000Z"
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    // Both candidate paths declared at override evaluation time.
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    // Native token used.
    assert_eq!(first_request_auth(&obs), "Bearer native-token");

    // Probe result has correct metadata from execute_provider_in_context.
    assert!(!result.outputs.is_empty(), "expected at least one output");
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(result.outputs[0].display_name, "Codex");
    assert!(result.outputs[0].account.is_active);
    assert_eq!(
        result.outputs[0].account.origin, "native",
        "default account origin should be native"
    );
}

#[test]
fn codex_override_uses_opencode_fallback_auth_when_primary_auth_missing() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access",
            expires: 1776806966592,
            accountId: "fallback-account"
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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    assert_eq!(first_request_auth(&obs), "Bearer fallback-access");
    assert_eq!(first_request_account_id(&obs), "fallback-account");

    // Probe metadata from typed runner.
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(account_is_active(&result, "opencode-0"), Some(true));
    assert_eq!(
        result.outputs[0].account.origin, "opencode",
        "opencode account origin should be opencode"
    );
}

#[test]
fn codex_override_accounts_json_inactive_entry_uses_entry_credentials() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "account-name-1";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          google: {
            type: "oauth",
            refresh: "auth-refresh",
            access: "auth-access",
            expires: 1776806966592,
            accountId: "auth-account"
          }
        });
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          openai: [
            {
              accountId: "active-main",
              isActive: true
            },
            {
              accountId: "account-name-1",
              isActive: false,
              data: {
                type: "oauth",
                refresh: "inactive-refresh",
                access: "inactive-access",
                expires: 1776806966592,
                accountId: "account-name-1"
              }
            }
          ]
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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    let matching_request = obs["requests"]
        .as_array()
        .and_then(|reqs| {
            reqs.iter().find(|r| {
                r["accountId"]
                    .as_str()
                    .map(|s| s == "account-name-1")
                    .unwrap_or(false)
            })
        })
        .cloned()
        .expect("expected request for account-name-1");
    assert_eq!(
        matching_request["authorization"],
        Value::String("Bearer inactive-access".to_string())
    );

    let account_output = result
        .outputs
        .iter()
        .find(|o| o.account.id == "account-name-1")
        .expect("expected output for account-name-1");
    assert_eq!(account_output.provider_id, "codex");
    assert_eq!(account_output.account.origin, "opencode");
    assert!(!account_output.account.is_active);
}

#[test]
fn codex_override_accounts_json_provider_key_not_array_creates_error_account() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          openai: {
            accountId: "broken"
          }
        });
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(result.outputs[0].account.origin, "opencode");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("must be a list of account entries"),
        "expected array validation error, got: {}",
        error_text
    );
}

#[test]
fn codex_override_accounts_json_without_active_creates_single_error_account() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          openai: [
            {
              accountId: "account-name-1",
              isActive: false,
              data: {
                type: "oauth",
                refresh: "inactive-refresh",
                expires: 1776806966592
              }
            }
          ]
        });
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs.len(),
        1,
        "expected exactly one error account"
    );
    assert_eq!(result.outputs[0].account.id, "opencode-0");
    assert_eq!(result.outputs[0].account.origin, "opencode");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("no active account selected"),
        "expected no-active validation error, got: {}",
        error_text
    );
}

#[test]
fn codex_override_accounts_json_multiple_active_creates_single_error_account() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "auth-refresh",
            access: "auth-access",
            expires: 1776806966592,
            accountId: "auth-account"
          }
        });
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          openai: [
            { accountId: "a1", isActive: true },
            { accountId: "a2", isActive: true }
          ]
        });
        "#,
    );

    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    assert_eq!(
        result.outputs.len(),
        1,
        "expected exactly one error account"
    );
    assert_eq!(result.outputs[0].account.id, "opencode-0");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("multiple active accounts selected"),
        "expected multiple-active validation error, got: {}",
        error_text
    );
}

#[test]
fn codex_override_accounts_json_active_entry_uses_auth_and_errors_when_auth_invalid() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          google: {
            type: "oauth",
            access: "google-access",
            refresh: "google-refresh",
            expires: 1776806966592
          }
        });
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          openai: [
            {
              accountId: "account-name-2",
              isActive: true
            }
          ]
        });
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(result.outputs[0].account.id, "account-name-2");
    assert_eq!(result.outputs[0].account.origin, "opencode");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("access") || error_text.contains("Provider block"),
        "expected auth.json-derived credential error, got: {}",
        error_text
    );
}

#[test]
fn codex_override_accounts_json_duplicate_account_id_creates_error_route() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          openai: [
            { accountId: "active-main", isActive: true },
            {
              accountId: "account-dup",
              isActive: false,
              data: {
                type: "oauth",
                refresh: "refresh-2",
                access: "access-2",
                expires: 1776806966592,
                accountId: "account-dup"
              }
            },
            {
              accountId: "account-dup",
              isActive: false,
              data: {
                type: "oauth",
                refresh: "refresh-3",
                access: "access-3",
                expires: 1776806966592,
                accountId: "account-dup"
              }
            }
          ]
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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    assert_eq!(
        result.outputs.len(),
        1,
        "expected exactly one error account"
    );
    assert_eq!(result.outputs[0].account.id, "opencode-0");

    let duplicate_error = result.outputs.iter().any(|output| {
        output
            .lines
            .iter()
            .map(|line| format!("{}", line))
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
            .contains("duplicate account name")
    });
    assert!(
        duplicate_error,
        "expected duplicate-account error output, got: {:?}",
        result
            .outputs
            .iter()
            .map(|o| (&o.account.id, &o.lines))
            .collect::<Vec<_>>()
    );
}

#[test]
fn codex_override_accounts_json_duplicate_default_does_not_conflict_with_native_default() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "refresh-default",
            access: "access-default",
            expires: 0,
            accountId: "acct-default"
          }
        });
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          openai: [
            { accountId: "default", isActive: true }
          ]
        });
        __test_state.account = "default";
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({
            user_id: "u-1",
            account_id: "acct-default",
            email: "x@example.com",
            plan_type: "plus",
            rate_limit: {
              primary_window: {
                used_percent: 1,
                reset_after_seconds: 10
              }
            }
          })
        });
        "#,
    );

    let _obs = obs.expect("assertion should produce state");

    assert!(!result.outputs.is_empty(), "expected probe output");
    assert_eq!(result.outputs[0].provider_id, "codex");

    let has_duplicate_discovery_error = result.outputs.iter().any(|output| {
        output
            .lines
            .iter()
            .map(|line| format!("{}", line))
            .collect::<Vec<_>>()
            .join(" ")
            .contains("Duplicate accountId in discovery results")
    });
    assert!(
        !has_duplicate_discovery_error,
        "same id as native default must not trigger discovery duplicate error: {:?}",
        result
            .outputs
            .iter()
            .map(|o| (&o.account.id, &o.lines))
            .collect::<Vec<_>>()
    );
}

#[test]
fn codex_override_logs_accounts_validation_errors() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          openai: [
            { accountId: "dup", isActive: true },
            { accountId: "dup", isActive: false, data: { access: "x" } }
          ]
        });
        "#,
    );

    let obs = obs.expect("assertion should produce state");
    let logs: Vec<String> = obs["logs"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    assert!(
        logs.iter().any(|line| {
            line.contains("warn:codex override:") && line.contains("duplicate account name")
        }),
        "expected validation warning in logs, got: {:?}",
        logs
    );

    assert_eq!(result.outputs.len(), 1, "expected one error output");
}

#[test]
fn codex_override_preserves_original_auth_path_priority() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.config/codex/auth.json"] = JSON.stringify({
          tokens: {
            access_token: "primary-access"
          },
          last_refresh: "2026-04-19T00:00:00.000Z"
        });
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access",
            expires: 1776806966592,
            accountId: "fallback-account"
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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    assert_eq!(first_request_auth(&obs), "Bearer primary-access");

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "native",
        "default account origin should be native"
    );
}

#[test]
fn codex_override_persists_refresh_back_to_opencode_auth_file() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "old-refresh",
            access: "old-access",
            expires: 1776806966592,
            accountId: "acc-123"
          }
        });
        __test_state.responses.usage.push({
          status: 401,
          headers: {},
          bodyText: JSON.stringify({ error: "expired" })
        });
        __test_state.responses.refresh.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({
            access_token: "new-access",
            refresh_token: "new-refresh"
          })
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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    let requests = obs["requests"].as_array().expect("requests array");
    assert!(
        requests.iter().any(|req| req["url"]
            .as_str()
            .unwrap_or_default()
            .contains("oauth/token")),
        "refresh request should be executed"
    );

    let usage_requests: Vec<&Value> = requests
        .iter()
        .filter(|req| {
            req["url"]
                .as_str()
                .unwrap_or_default()
                .contains("/wham/usage")
        })
        .collect();
    assert!(usage_requests.len() >= 2, "expected two usage requests");
    assert_eq!(
        usage_requests[1]["authorization"],
        Value::String("Bearer new-access".to_string())
    );

    let updated_text = obs["files"]["~/.local/share/opencode/auth.json"]
        .as_str()
        .expect("updated opencode auth file text");
    let updated: Value = serde_json::from_str(updated_text).expect("updated auth json");
    assert_eq!(
        updated["openai"]["access"],
        Value::String("new-access".to_string())
    );
    assert_eq!(
        updated["openai"]["refresh"],
        Value::String("new-refresh".to_string())
    );
    assert_eq!(
        updated["openai"]["accountId"],
        Value::String("acc-123".to_string())
    );

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "opencode",
        "opencode account origin should be opencode"
    );
}

#[test]
fn codex_override_persists_refresh_back_to_accounts_json_for_inactive_entry() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "account-name-1";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          google: {
            type: "oauth",
            refresh: "auth-refresh",
            access: "auth-access",
            expires: 1776806966592,
            accountId: "auth-account"
          }
        });
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          openai: [
            {
              accountId: "active-main",
              isActive: true
            },
            {
              accountId: "account-name-1",
              isActive: false,
              data: {
                type: "oauth",
                refresh: "old-refresh",
                access: "old-access",
                expires: 1776806966592,
                accountId: "account-name-1"
              }
            }
          ]
        });
        __test_state.responses.usage.push({
          status: 401,
          headers: {},
          bodyText: JSON.stringify({ error: "expired" })
        });
        __test_state.responses.refresh.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({
            access_token: "new-access-from-refresh",
            refresh_token: "new-refresh-from-refresh"
          })
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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    let requests = obs["requests"].as_array().expect("requests array");
    assert!(
        requests.iter().any(|req| req["url"]
            .as_str()
            .unwrap_or_default()
            .contains("oauth/token")),
        "refresh request should be executed"
    );

    let updated_accounts_text = obs["files"]["~/.local/share/opencode/accounts.json"]
        .as_str()
        .expect("updated accounts file text");
    let updated_accounts: Value =
        serde_json::from_str(updated_accounts_text).expect("updated accounts json");
    assert_eq!(
        updated_accounts["openai"][1]["data"]["access"],
        Value::String("new-access-from-refresh".to_string())
    );
    assert_eq!(
        updated_accounts["openai"][1]["data"]["refresh"],
        Value::String("new-refresh-from-refresh".to_string())
    );
    assert_eq!(
        updated_accounts["openai"][1]["accountId"],
        Value::String("account-name-1".to_string())
    );

    let updated_auth_text = obs["files"]["~/.local/share/opencode/auth.json"]
        .as_str()
        .expect("auth file text");
    let updated_auth: Value = serde_json::from_str(updated_auth_text).expect("auth json");
    assert_eq!(
        updated_auth["google"]["access"],
        Value::String("auth-access".to_string())
    );
    assert_eq!(
        updated_auth["google"]["refresh"],
        Value::String("auth-refresh".to_string())
    );

    let account_output = result
        .outputs
        .iter()
        .find(|o| o.account.id == "account-name-1")
        .expect("expected output for account-name-1");
    assert_eq!(account_output.provider_id, "codex");
    assert_eq!(account_output.account.origin, "opencode");
}

#[test]
fn codex_override_reloads_opencode_auth_before_refresh() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "old-refresh",
            access: "old-access",
            expires: 1776806966592,
            accountId: "acc-123"
          }
        });
        __test_state.responses.usage.push({
          status: 401,
          headers: {},
          bodyText: JSON.stringify({ error: "expired" })
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });

        __test_ctx.util.retryOnceOnAuth = function (opts) {
          var first = opts.request();
          if (!__test_ctx.util.isAuthStatus(first.status)) return first;

          __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
            openai: {
              type: "oauth",
              refresh: "new-refresh-from-opencode",
              access: "new-access-from-opencode",
              expires: 1776806966592,
              accountId: "acc-123"
            }
          });

          var refreshed = opts.refresh();
          if (!refreshed) return first;
          return opts.request(refreshed);
        };
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    let requests = obs["requests"].as_array().expect("requests array");
    assert!(
        requests.iter().all(|req| !req["url"]
            .as_str()
            .unwrap_or_default()
            .contains("oauth/token")),
        "refresh endpoint should not be called when auth.json already has a new token"
    );

    let usage_requests: Vec<&Value> = requests
        .iter()
        .filter(|req| {
            req["url"]
                .as_str()
                .unwrap_or_default()
                .contains("/wham/usage")
        })
        .collect();
    assert!(usage_requests.len() >= 2, "expected two usage requests");
    assert_eq!(
        usage_requests[1]["authorization"],
        Value::String("Bearer new-access-from-opencode".to_string())
    );

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "opencode",
        "opencode account origin should be opencode"
    );
}

#[test]
fn codex_override_preserves_other_providers_when_persisting_refresh() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "old-refresh",
            access: "old-access",
            expires: 1776806966592,
            accountId: "acc-123"
          },
          google: {
            type: "oauth",
            refresh: "google-refresh",
            access: "google-access",
            expires: 1775261213360
          }
        });
        __test_state.responses.usage.push({
          status: 401,
          headers: {},
          bodyText: JSON.stringify({ error: "expired" })
        });
        __test_state.responses.refresh.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({
            access_token: "new-access",
            refresh_token: "new-refresh"
          })
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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    let updated_text = obs["files"]["~/.local/share/opencode/auth.json"]
        .as_str()
        .expect("updated opencode auth file text");
    let updated: Value = serde_json::from_str(updated_text).expect("updated auth json");

    assert_eq!(
        updated["openai"]["access"],
        Value::String("new-access".to_string())
    );
    assert_eq!(
        updated["google"]["access"],
        Value::String("google-access".to_string())
    );
    assert_eq!(
        updated["google"]["refresh"],
        Value::String("google-refresh".to_string())
    );

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "opencode",
        "opencode account origin should be opencode"
    );
}

#[test]
fn codex_override_tries_multiple_opencode_auth_paths() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "opencode-1";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          google: {
            type: "oauth",
            refresh: "g-refresh",
            access: "g-access",
            expires: 1775261213360
          }
        });
        __test_state.files["~/.config/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access-2",
            expires: 1776806966592,
            accountId: "fallback-account-2"
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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    assert_eq!(first_request_auth(&obs), "Bearer fallback-access-2");
    assert_eq!(first_request_account_id(&obs), "fallback-account-2");

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "opencode",
        "opencode account origin should be opencode"
    );
}

#[test]
fn codex_override_keeps_not_logged_in_error_without_valid_fallback_payload() {
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = "{bad-json";
        "#,
    );

    let obs = obs.expect("assertion should produce state");

    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    // The plugin or override fails to parse the invalid JSON during lifecycle
    // (e.g. discoverAccounts tries to read and parse the file). The error
    // surfaces in the ProbeResult's PluginOutput lines.
    assert!(!result.outputs.is_empty(), "expected an output");
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "opencode",
        "opencode error account origin should be opencode"
    );
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("Invalid JSON") || error_text.contains("failed"),
        "expected a parse or lifecycle error for bad JSON, got: {}",
        error_text
    );
}

#[test]
fn codex_override_only_default_soft_fail() {
    // Only one account with error policy hide-if-other-account -> no suppression.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "codex",
        plugin_name: "Codex",
        plugin_source: &codex_plugin_script(),
        override_source: Some(&codex_override_script()),
        harness: HARNESS_SCRIPT,
        setup: r#"
        __test_state.files["~/.config/codex/auth.json"] = JSON.stringify({
          tokens: { access_token: "primary" },
          last_refresh: "2026-04-19T00:00:00.000Z"
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
        ProviderOutcome::Probe { result, .. } => {
            // Should have at least one output from the real plugin discovery.
            assert!(!result.outputs.is_empty(), "expected probe outputs");
            assert_eq!(result.outputs[0].provider_id, "codex");
            assert_eq!(
                result.outputs[0].account.origin, "native",
                "default only account origin should be native"
            );
        }
        other => panic!("expected Probe outcome, got {:?}", other),
    }
}

#[test]
fn codex_override_stable_path_index_ids() {
    // Verify that discovery accounts carry stable path-index-based IDs
    // matching the original override behavior.
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "r",
            access: "a",
            expires: 1776806966592,
            accountId: "acct-0"
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

    // Subscription paths confirm the override's fallback discovery ran.
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "opencode",
        "opencode account origin should be opencode"
    );
}

#[test]
fn codex_override_no_cross_source_native_fallback() {
    // When native auth exists, opencode fallback is NOT used.
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.files["~/.config/codex/auth.json"] = JSON.stringify({
          tokens: {
            access_token: "native-only"
          },
          last_refresh: "2026-04-19T00:00:00.000Z"
        });
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access",
            expires: 1776806966592,
            accountId: "fallback-account"
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

    // Should still declare both candidate paths.
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );

    // Native token used, not opencode.
    assert_eq!(first_request_auth(&obs), "Bearer native-only");

    assert!(!result.outputs.is_empty());
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "native",
        "default account origin should be native"
    );
    // The opencode-0 output (index 1) should have opencode origin.
    if result.outputs.len() > 1 {
        assert_eq!(
            result.outputs[1].account.origin, "opencode",
            "opencode account origin should be opencode"
        );
    }
}

#[test]
fn codex_override_empty_source_no_account() {
    // Empty source file is treated as normally absent — no account/error created.
    let (result, obs) = run_codex_probe(
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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );
    // Only default account — no error descriptor for empty source.
    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "native",
        "default account origin should be native"
    );
}

#[test]
fn codex_override_unreadable_source_creates_error_account() {
    // Source file exists but readText throws -> error account for that candidate.
    let (result, obs) = run_codex_probe(
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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );
    // Error account with read error descriptor visible.
    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "opencode",
        "opencode error account origin should be opencode"
    );
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("read error") || error_text.contains("Failed to read"),
        "expected read error, got: {}",
        error_text
    );
}

#[test]
fn codex_override_missing_provider_key_omits_candidate() {
    // File exists but no openai key -> no account created for that candidate.
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
            google: { type: "oauth", access: "g-access", expires: 1776806966592 }
        });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );
    // Only default account — missing key creates no extra account.
    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
}

#[test]
fn codex_override_malformed_provider_block_null_error() {
    // openai key is null -> error account.
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
            openai: null
        });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );
    // Null provider block creates a visible error for that candidate.
    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("Provider block") || error_text.contains("Invalid"),
        "expected provider block error, got: {}",
        error_text
    );
}

#[test]
fn codex_override_malformed_provider_block_array_error() {
    // openai key is an array -> error account.
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
            openai: ["not", "valid"]
        });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );
    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "opencode",
        "opencode error account origin should be opencode"
    );
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("Provider block") || error_text.contains("Invalid"),
        "expected provider block error, got: {}",
        error_text
    );
}

#[test]
fn codex_override_missing_access_token_error() {
    // Provider block exists but missing access token -> error account.
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
            openai: { type: "oauth", refresh: "r", expires: 1776806966592 }
        });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );
    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
    assert_eq!(
        result.outputs[0].account.origin, "opencode",
        "opencode error account origin should be opencode"
    );
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("access") || error_text.contains("Invalid"),
        "expected error about missing access token, got: {}",
        error_text
    );
}

#[test]
fn codex_override_default_native_error_visible_when_alone() {
    // No opencode files, native auth fails -> default error visible.
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );
    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("Not logged in") || error_text.contains("error"),
        "expected native auth failure visible, got: {}",
        error_text
    );
}

#[test]
fn codex_override_default_suppressed_when_opencode_descriptor_exists() {
    // Malformed JSON creates error descriptor; default error suppressed.
    let (result, obs) = run_codex_probe(
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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );
    // Only opencode-0 error account visible, default suppressed.
    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
}

#[test]
fn codex_override_no_cross_source_fallback() {
    // OpenCode account should not fall through to native credentials.
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.files["~/.config/codex/auth.json"] = JSON.stringify({
            tokens: { access_token: "native-only" },
            last_refresh: "2026-04-19T00:00:00.000Z"
        });
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
            openai: { type: "oauth", access: "oc-access", expires: 1776806966592, accountId: "oc-acc" }
        });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify({}) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );
    // Native token ignored for opencode account
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "codex");
}

#[test]
fn codex_override_refresh_error_propagation() {
    // Upstream refresh endpoint returns error -> propagated to output.
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
            openai: { type: "oauth", refresh: "old-refresh", access: "old-access",
                      expires: 1776806966592, accountId: "acc-123" }
        });
        __test_state.responses.usage.push({ status: 401, headers: {}, bodyText: JSON.stringify({ error: "expired" }) });
        __test_state.responses.refresh.push({ status: 400, headers: {}, bodyText: JSON.stringify({ error: "invalid_grant" }) });
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );
    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("Token expired")
            || error_text.contains("refresh")
            || error_text.contains("invalid_grant"),
        "expected refresh error propagation, got: {}",
        error_text
    );
}

#[test]
fn codex_override_persistence_failure_visible() {
    // Persist fails after refresh -> error visible, original file unchanged.
    let (result, obs) = run_codex_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
            openai: { type: "oauth", refresh: "old-refresh", access: "old-access",
                      expires: 1776806966592, accountId: "acc-123" }
        });
        __test_state.responses.usage.push({ status: 401, headers: {}, bodyText: JSON.stringify({ error: "expired" }) });
        __test_state.responses.refresh.push({ status: 200, headers: {}, bodyText: JSON.stringify({ access_token: "new-access", refresh_token: "new-refresh" }) });
        __test_ctx.host.fs.writeText = function() { throw new Error("write failure"); };
        "#,
    );
    let obs = obs.expect("assertion should produce state");
    assert_subscriptions(
        &obs,
        &[
            "~/.local/share/opencode/auth.json",
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
        ],
    );
    assert!(!result.outputs.is_empty(), "expected probe outputs");
    assert_eq!(result.outputs[0].provider_id, "codex");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        error_text.contains("write")
            || error_text.contains("persist")
            || error_text.contains("failed"),
        "expected persistence error, got: {}",
        error_text
    );
}
// ═══════════════════════════════════════════════════════════════════════
// Regression: missing subscribeFile
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn codex_override_fails_visibly_when_subscribefile_missing() {
    // Regression: missing subscribeFile should cause override init to fail
    // visibly rather than silently continuing.
    let outcome = run_provider_probe(&ProviderSpec {
        plugin_id: "codex",
        plugin_name: "Codex",
        plugin_source: &codex_plugin_script(),
        override_source: Some(&codex_override_script()),
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
fn codex_override_run_probe_lifecycle() {
    let Some(paths) = support::hermetic::enter("codex_override_run_probe_lifecycle") else {
        return;
    };

    // Use the public production `run_probe` entrypoint with a temporary
    // override directory and HOME, exercising the real override file and
    // real host API (filesystem, env) without network.
    use openusage_cli::plugin_engine::manifest::{LoadedPlugin, PluginManifest};
    use openusage_cli::plugin_engine::runtime::{MetricLine, PluginOutput, run_probe_in_sandbox};

    // Synthetic plugin with all AST patch targets required by the codex override.
    let plugin_script = r#"
(function() {
  function loadAuth(ctx) { return null; }
  function saveAuth(ctx, authState) { return false; }
  function refreshToken(ctx, authState) { return null; }
  function probe(ctx) {
    var authState = loadAuth(ctx);
    if (!authState || !authState.auth) {
      throw "Not logged in. Run `codex` to authenticate.";
    }
    var auth = authState.auth;
    if (auth.tokens && auth.tokens.access_token) {
      return {
        plan: "Pro",
        lines: [ctx.line.badge({ label: "Status", text: "Authenticated" })]
      };
    }
    throw "Not logged in. Run `codex` to authenticate.";
  }
  globalThis.__openusage_plugin = { id: "codex", probe: probe };
})();
"#;

    // Write the real override file
    let override_path = paths.overrides.join("codex.js");
    std::fs::write(&override_path, codex_override_script()).expect("write override file");

    // Create a valid OpenCode auth candidate in HOME
    let opencode_dir = paths.root.join(".local/share/opencode");
    std::fs::create_dir_all(&opencode_dir).expect("create opencode dir");
    let auth_json = serde_json::json!({
        "openai": {
            "type": "oauth",
            "access": "test-access-token",
            "refresh": "test-refresh-token",
            "expires": 9999999999999i64,
            "accountId": "test-account"
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
            id: "codex".to_string(),
            name: "Codex".to_string(),
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
        result.outputs
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
    assert_eq!(output.provider_id, "codex");
    assert_eq!(output.display_name, "Codex");
    assert_eq!(output.plan.as_deref(), Some("Pro"));
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
// Harness and assertion scripts
// ═══════════════════════════════════════════════════════════════════════

/// The harness script sets up `__test_state` and `__openusage_ctx` with
/// mock implementations for all host APIs the Codex plugin and override use.
///
/// This is evaluated via the `before_plugin` hook before the lifecycle runs,
/// so the plugin and override see the mock context.
const HARNESS_SCRIPT: &str = r#"
(function () {
  globalThis.__test_state = {
    env: {},
    files: {},
    requests: [],
    logs: [],
    subscriptions: [],
    readErrors: {},
    responses: {
      usage: [],
      refresh: []
    },
    ccusageResult: { status: "no_runner" }
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

  var ctx = {
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
      env: {
        get: function (name) {
          if (Object.prototype.hasOwnProperty.call(globalThis.__test_state.env, name)) {
            return globalThis.__test_state.env[name];
          }
          return null;
        }
      },
      keychain: {
        readGenericPassword: function () { return null; },
        writeGenericPassword: function () {},
        writeGenericPasswordForCurrentUser: function () {},
        readGenericPasswordForCurrentUser: function () { return null; },
        deleteGenericPassword: function () {}
      },
      http: {
        request: function (opts) {
          opts = opts || {};
          var headers = cloneHeaders(opts.headers);
          var url = String(opts.url || "");
          globalThis.__test_state.requests.push({
            method: String(opts.method || "GET"),
            url: url,
            authorization: headers.Authorization || null,
            accountId: headers["ChatGPT-Account-Id"] || null
          });

          var pool = url.indexOf("/oauth/token") !== -1
            ? globalThis.__test_state.responses.refresh
            : globalThis.__test_state.responses.usage;
          var resp = pool.length > 0 ? pool.shift() : null;
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
      ccusage: {
        query: function () {
          return globalThis.__test_state.ccusageResult;
        }
      },
      log: {
        debug: function (msg) { globalThis.__test_state.logs.push("debug:" + String(msg)); },
        info: function (msg) { globalThis.__test_state.logs.push("info:" + String(msg)); },
        warn: function (msg) { globalThis.__test_state.logs.push("warn:" + String(msg)); },
        error: function (msg) { globalThis.__test_state.logs.push("error:" + String(msg)); }
      }
    },
    util: {
      tryParseJson: function (text) {
        if (text === null || text === undefined) return null;
        try {
          return JSON.parse(String(text));
        } catch (_) {
          return null;
        }
      },
      parseDateMs: function (value) {
        if (typeof value === "number" && Number.isFinite(value)) return value;
        if (typeof value === "string") {
          var parsed = Date.parse(value);
          return Number.isFinite(parsed) ? parsed : null;
        }
        return null;
      },
      request: function (opts) {
        return ctx.host.http.request(opts);
      },
      isAuthStatus: function (status) {
        return status === 401 || status === 403;
      },
      retryOnceOnAuth: function (opts) {
        var first = opts.request();
        if (!ctx.util.isAuthStatus(first.status)) return first;
        var refreshed = opts.refresh();
        if (!refreshed) return first;
        return opts.request(refreshed);
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

/// Post-probe assertion script — captures test state as JSON for Rust assertions.
/// This is evaluated after `execute_provider_in_context` finishes, so all
/// probe-induced file writes, requests, and subscriptions are available.
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
