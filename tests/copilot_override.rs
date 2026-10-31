#![allow(unreachable_patterns)]

// Integration tests for the Copilot plugin override.
//
// Harness tests call the shared `ProviderSpec` runner,
// which calls `execute_provider_in_context` (production probe code).
// No manual Runtime/Context/override_api injection or discovery/probe loops.
//
// Tests assert actual `ProbeResult` (typed outputs with lines, accounts)
// and/or optional post-probe assertion JS that captures test-state JSON.

mod support;

use openusage_cli::config::{AuthSource, DEFAULT_AUTH_SOURCES};
use openusage_cli::plugin_engine::runtime::{MetricLine, ProbeResult, ProgressFormat};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

use support::override_runner::{
    ProviderOutcome, ProviderSpec, run_provider_probe, run_provider_probe_with_auth_sources,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn copilot_plugin_script() -> String {
    let path = repo_root().join("vendor/crossusage/plugins/copilot/plugin.js");
    fs::read_to_string(path).expect("read copilot plugin")
}

fn copilot_override_script() -> String {
    let path = repo_root().join("plugin-overrides/copilot.js");
    fs::read_to_string(path).expect("read copilot override")
}

/// Run a copilot probe test through the shared runner.
fn run_copilot_probe(setup_js: &str) -> (ProbeResult, Option<Value>) {
    run_copilot_probe_with_sources(setup_js, DEFAULT_AUTH_SOURCES)
}

fn run_copilot_probe_with_sources(
    setup_js: &str,
    sources: &[AuthSource],
) -> (ProbeResult, Option<Value>) {
    let spec = ProviderSpec {
        plugin_id: "copilot",
        plugin_name: "Copilot",
        plugin_source: &copilot_plugin_script(),
        override_source: Some(&copilot_override_script()),
        harness: HARNESS_SCRIPT,
        setup: setup_js,
        assertion: Some(ASSERTION_SCRIPT),
    };
    let outcome = if sources == DEFAULT_AUTH_SOURCES {
        run_provider_probe(&spec)
    } else {
        run_provider_probe_with_auth_sources(&spec, sources)
    };
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
    let mut expected = expected.to_vec();
    expected.push("~/.pi/agent/auth.json");
    assert_eq!(subs, expected, "subscription mismatch");
    assert_eq!(obs["writes"], serde_json::json!([]), "no credential writes");
}

fn assert_requests(obs: &Value, tokens: &[&str]) {
    let requests = obs["requests"].as_array().expect("requests");
    assert_eq!(requests.len(), tokens.len(), "exact request count");
    for (request, token) in requests.iter().zip(tokens) {
        assert_eq!(request["method"], "GET");
        assert_eq!(
            request["url"],
            "https://api.github.com/copilot_internal/user"
        );
        assert_eq!(request["authorization"], format!("token {token}"));
    }
    assert_eq!(obs["writes"], serde_json::json!([]), "no credential writes");
}

fn assert_credits(result: &ProbeResult, id: &str, origin: &str, name: Option<&str>, active: bool) {
    let output = result
        .outputs
        .iter()
        .find(|o| o.account.id == id)
        .unwrap_or_else(|| panic!("missing {id}: {:?}", result.outputs));
    assert_eq!(output.provider_id, "copilot");
    assert_eq!(output.display_name, "Copilot");
    assert_eq!(output.account.origin, origin);
    assert_eq!(output.account.name.as_deref(), name);
    assert_eq!(output.account.is_active, active);
    assert_eq!(output.plan.as_deref(), Some("Pro"));
    assert_eq!(output.lines.len(), 1, "exact successful metric count");
    match &output.lines[0] {
        MetricLine::Progress {
            label,
            used,
            limit,
            format,
            resets_at,
            period_duration_ms,
            ..
        } => {
            assert_eq!(label, "Credits");
            assert_eq!(*used, 25.0);
            assert_eq!(*limit, 100.0);
            assert!(matches!(format, ProgressFormat::Percent));
            assert_eq!(resets_at.as_deref(), Some("2099-01-15T00:00:00.000Z"));
            assert_eq!(*period_duration_ms, Some(2_592_000_000));
        }
        other => panic!("expected non-error Credits quota for {id}, got {other:?}"),
    }
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

fn account_is_active(result: &ProbeResult, account_id: &str) -> Option<bool> {
    result
        .outputs
        .iter()
        .find(|output| output.account.id == account_id)
        .map(|output| output.account.is_active)
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
fn copilot_override_private_state_stays_inside_closure() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.inspectPrivacy = true;
        __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({token: "native-token"});
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({"github-copilot": {access: "opencode-token"}});
        __test_state.responses.usage.push({status: 200, bodyText: JSON.stringify(__test_state.successUsage)});
        __test_state.responses.usage.push({status: 200, bodyText: JSON.stringify(__test_state.successUsage)});
    "#,
    );
    let obs = obs.expect("privacy observation");
    for binding in obs["privacy"]
        .as_array()
        .expect("private bindings checked during native request")
    {
        assert_eq!(
            binding["visibility"], "undefined",
            "private binding leaked: {}",
            binding["name"]
        );
    }
    assert_eq!(result.outputs.len(), 2);
    assert_credits(&result, "default", "native", None, true);
    assert_credits(&result, "opencode-0", "opencode", Some("OpenCode 2"), true);
    assert_requests(&obs, &["native-token", "opencode-token"]);
}

#[test]
fn copilot_override_http_200_without_usage_is_an_error() {
    for sources in [&[AuthSource::Native][..], &[AuthSource::Opencode][..]] {
        let (result, obs) = run_copilot_probe_with_sources(
            r#"
            __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({token: "native-token"});
            __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({"github-copilot": {access: "opencode-token"}});
            __test_state.responses.usage.push({status: 200, headers: {}, bodyText: "{}"});
        "#,
            sources,
        );
        assert_eq!(
            result.outputs.len(),
            1,
            "selected account error is not suppressed by disabled accounts"
        );
        let output = &result.outputs[0];
        let (id, origin, token) = if sources == [AuthSource::Native] {
            ("default", "native", "native-token")
        } else {
            ("opencode-0", "opencode", "opencode-token")
        };
        assert_eq!(output.account.id, id);
        assert_eq!(output.account.origin, origin);
        match &output.lines[..] {
            [MetricLine::Badge { label, text, .. }] => {
                assert_eq!(label, "Error");
                assert_eq!(text, "Copilot usage data is unavailable for this account.");
            }
            other => panic!("expected no-data error, got {other:?}"),
        }
        assert_requests(&obs.expect("request observation"), &[token]);
    }
}

#[test]
fn copilot_override_selected_opencode_auth_error_never_uses_native_or_other_file() {
    let (result, obs) = run_copilot_probe_with_sources(
        r#"
        __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({token: "native-token"});
        __test_state.keychain["gh:github.com"] = "gh-token";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({"github-copilot": {access: "selected-token"}});
        __test_state.files["~/.config/opencode/auth.json"] = JSON.stringify({"github-copilot": {access: "other-token"}});
        __test_state.responses.usage.push({status: 401, bodyText: "{}"});
        __test_state.responses.usage.push({status: 200, bodyText: JSON.stringify(__test_state.successUsage)});
    "#,
        &[AuthSource::Opencode],
    );
    let obs = obs.expect("request observation");
    assert_eq!(result.outputs.len(), 2);
    let selected = &result.outputs[0];
    assert_eq!(selected.account.id, "opencode-0");
    assert_eq!(selected.account.origin, "opencode");
    match &selected.lines[..] {
        [MetricLine::Badge { label, text, .. }] => {
            assert_eq!(label, "Error");
            assert_eq!(
                text,
                "Token invalid. Run `gh auth login` to re-authenticate."
            );
        }
        other => panic!("expected selected source failure, got {other:?}"),
    }
    assert_credits(&result, "opencode-1", "opencode", Some("OpenCode 3"), true);
    assert_requests(&obs, &["selected-token", "other-token"]);
}

#[test]
fn copilot_override_filters_auth_sources_before_requests() {
    let setup = r#"
        __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({token: "native-token"});
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({"github-copilot": {access: "opencode-token"}});
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({"github-copilot": [
            {accountId: "default", isActive: true},
            {accountId: "inactive", isActive: false, data: {access: "inactive-token"}}
        ]});
        for (var i = 0; i < 3; i++) {
            __test_state.responses.usage.push({status: 200, headers: {}, bodyText: JSON.stringify(__test_state.successUsage)});
        }
    "#;
    for (sources, tokens) in [
        (
            DEFAULT_AUTH_SOURCES,
            vec!["native-token", "opencode-token", "inactive-token"],
        ),
        (&[AuthSource::Native][..], vec!["native-token"]),
        (
            &[AuthSource::Opencode][..],
            vec!["opencode-token", "inactive-token"],
        ),
        (&[][..], vec![]),
    ] {
        let (result, observed) = run_copilot_probe_with_sources(setup, sources);
        let observed = observed.expect("request counters");
        let authorizations: Vec<&str> = observed["requests"]
            .as_array()
            .expect("requests")
            .iter()
            .map(|request| request["authorization"].as_str().expect("authorization"))
            .collect();
        let expected: Vec<String> = tokens
            .iter()
            .map(|token| format!("token {token}"))
            .collect();
        assert_eq!(authorizations, expected);
        assert_eq!(result.outputs.len(), tokens.len());
        assert_requests(&observed, &tokens);
        for output in &result.outputs {
            let (origin, name, active) = match output.account.id.as_str() {
                "default" => ("native", None, true),
                "opencode-0" => ("opencode", Some("default"), true),
                "inactive" => ("opencode", Some("inactive"), false),
                other => panic!("unexpected account {other}"),
            };
            assert_credits(&result, &output.account.id, origin, name, active);
        }
        assert_subscriptions(
            &observed,
            &[
                "~/.local/share/opencode/auth.json",
                "~/.local/share/opencode/accounts.json",
                "~/.config/opencode/auth.json",
                "~/.config/opencode/accounts.json",
            ],
        );
        if sources == [AuthSource::Opencode] {
            assert!(
                result
                    .outputs
                    .iter()
                    .all(|o| o.account.origin == "opencode")
            );
            assert_eq!(result.outputs[0].account.name.as_deref(), Some("default"));
            assert_ne!(result.outputs[0].account.id, "default");
        }
    }
}

#[test]
fn copilot_override_native_default_source() {
    // Native Copilot keychain auth (no opencode fallback).
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({
          token: "native-token"
        });
        // percent_remaining takes precedence over the entitlement fallback.
        __test_state.successUsage.quota_snapshots.premium_interactions.percent_remaining = 75;
        __test_state.successUsage.quota_snapshots.premium_interactions.remaining = 0;
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify(__test_state.successUsage)
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

    assert_eq!(first_request_auth(&obs), "token native-token");
    assert_eq!(result.outputs.len(), 1);
    assert_credits(&result, "default", "native", None, true);
    assert_requests(&obs, &["native-token"]);

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");
    assert!(result.outputs[0].account.is_active);

    // Native default account retains its origin.
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
          bodyText: JSON.stringify(__test_state.successUsage)
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

    assert_eq!(first_request_auth(&obs), "token fallback-access");
    assert_eq!(result.outputs.len(), 1);
    assert_credits(&result, "opencode-0", "opencode", Some("OpenCode 2"), true);
    assert_requests(&obs, &["fallback-access"]);

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");
    assert_eq!(account_is_active(&result, "opencode-0"), Some(true));

    // OpenCode account retains its source origin.
    assert_opencode_origin(&result, "opencode-0");
}

#[test]
fn copilot_override_accounts_json_inactive_entry_uses_entry_credentials() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.account = "account-name-1";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          google: {
            type: "oauth",
            refresh: "auth-refresh",
            access: "auth-access",
            expires: 0
          }
        });
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          "github-copilot": [
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
                expires: 0
              }
            }
          ]
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify(__test_state.successUsage)
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
                r["authorization"]
                    .as_str()
                    .map(|s| s == "token inactive-access")
                    .unwrap_or(false)
            })
        })
        .cloned()
        .expect("expected request with inactive token");
    assert_eq!(
        matching_request["authorization"],
        Value::String("token inactive-access".to_string())
    );

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");
    assert_opencode_origin(&result, "account-name-1");
    assert_eq!(
        result.outputs.len(),
        2,
        "active error plus inactive success"
    );
    assert_credits(
        &result,
        "account-name-1",
        "opencode",
        Some("account-name-1"),
        false,
    );
    assert_requests(&obs, &["inactive-access"]);
    assert_eq!(account_is_active(&result, "account-name-1"), Some(false));
}

#[test]
fn copilot_override_accounts_json_active_entry_uses_auth_json_credentials() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.account = "account-name-2";
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          "github-copilot": {
            type: "oauth",
            refresh: "auth-refresh",
            access: "auth-access",
            expires: 0
          }
        });
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          "github-copilot": [
            {
              accountId: "account-name-2",
              isActive: true,
              data: {
                type: "oauth",
                refresh: "inactive-refresh",
                access: "inactive-access",
                expires: 0
              }
            }
          ]
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify(__test_state.successUsage)
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

    assert_eq!(first_request_auth(&obs), "token auth-access");
    assert_eq!(result.outputs.len(), 1);
    assert_credits(
        &result,
        "account-name-2",
        "opencode",
        Some("account-name-2"),
        true,
    );
    assert_requests(&obs, &["auth-access"]);

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");
    assert_opencode_origin(&result, "account-name-2");
}

#[test]
fn copilot_override_accounts_json_provider_key_not_array_creates_error_account() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          "github-copilot": {
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
    assert_eq!(result.outputs[0].provider_id, "copilot");
    assert_eq!(result.outputs[0].account.origin, "opencode");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(result.outputs.len(), 1);
    assert_requests(&obs, &[]);
    assert!(
        error_text.contains("must be a list of account entries"),
        "expected array validation error, got: {}",
        error_text
    );
}

#[test]
fn copilot_override_accounts_json_without_active_creates_single_error_account() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          "github-copilot": [
            {
              accountId: "account-name-1",
              isActive: false,
              data: {
                type: "oauth",
                refresh: "inactive-refresh",
                expires: 0
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
    assert_eq!(result.outputs[0].provider_id, "copilot");
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
    assert_requests(&obs, &[]);
    assert!(
        error_text.contains("no active account selected"),
        "expected no-active validation error, got: {}",
        error_text
    );
}

#[test]
fn copilot_override_accounts_json_multiple_active_creates_single_error_account() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          "github-copilot": {
            type: "oauth",
            refresh: "auth-refresh",
            access: "auth-access",
            expires: 0
          }
        });
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          "github-copilot": [
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
    assert_requests(&obs, &[]);
    assert!(
        error_text.contains("multiple active accounts selected"),
        "expected multiple-active validation error, got: {}",
        error_text
    );
}

#[test]
fn copilot_override_accounts_json_active_entry_uses_auth_and_errors_when_auth_invalid() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          google: {
            type: "oauth",
            access: "google-access",
            refresh: "google-refresh",
            expires: 1
          }
        });
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          "github-copilot": [
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
    assert_eq!(result.outputs[0].provider_id, "copilot");
    assert_eq!(result.outputs[0].account.id, "account-name-2");
    assert_eq!(result.outputs[0].account.origin, "opencode");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(result.outputs.len(), 1);
    assert_requests(&obs, &[]);
    assert!(
        error_text.contains("Invalid") || error_text.contains("credentials"),
        "expected auth.json-derived credential error, got: {}",
        error_text
    );
}

#[test]
fn copilot_override_accounts_json_duplicate_account_id_creates_error_route() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          "github-copilot": [
            { accountId: "active-main", isActive: true },
            {
              accountId: "account-dup",
              isActive: false,
              data: {
                type: "oauth",
                refresh: "refresh-2",
                access: "access-2",
                expires: 0
              }
            },
            {
              accountId: "account-dup",
              isActive: false,
              data: {
                type: "oauth",
                refresh: "refresh-3",
                access: "access-3",
                expires: 0
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
    assert_requests(&obs, &[]);
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
fn copilot_override_accounts_json_duplicate_default_does_not_conflict_with_native_default() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          "github-copilot": {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access",
            expires: 0
          }
        });
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          "github-copilot": [
            { accountId: "default", isActive: true }
          ]
        });
        __test_state.account = "default";
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify(__test_state.successUsage)
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

    assert!(!result.outputs.is_empty(), "expected probe output");
    assert_eq!(result.outputs[0].provider_id, "copilot");

    let has_duplicate_discovery_error = result.outputs.iter().any(|output| {
        output
            .lines
            .iter()
            .map(|line| format!("{}", line))
            .collect::<Vec<_>>()
            .join(" ")
            .contains("Duplicate accountId in discovery results")
    });
    assert_eq!(result.outputs.len(), 1);
    assert_credits(&result, "opencode-0", "opencode", Some("default"), true);
    assert_requests(&obs, &["fallback-access"]);
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
fn copilot_override_logs_accounts_validation_errors() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/accounts.json"] = JSON.stringify({
          "github-copilot": [
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
            line.contains("warn:copilot override:") && line.contains("duplicate account name")
        }),
        "expected validation warning in logs, got: {:?}",
        logs
    );

    assert_eq!(result.outputs.len(), 1, "expected one error output");
    assert_requests(&obs, &[]);
}

#[test]
fn copilot_override_preserves_original_auth_priority() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({
          token: "primary-token"
        });
        __test_state.keychain["gh:github.com"] = "lower-priority-gh-token";
        __test_state.files["~/.config/openusage/plugins/copilot/auth.json"] = JSON.stringify({token: "lower-priority-state-token"});
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
          bodyText: JSON.stringify(__test_state.successUsage)
        }, {
          status: 200, headers: {}, bodyText: JSON.stringify(__test_state.successUsage)
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

    assert_eq!(first_request_auth(&obs), "token primary-token");
    assert_eq!(result.outputs.len(), 2);
    assert_credits(&result, "default", "native", None, true);
    assert_credits(&result, "opencode-0", "opencode", Some("OpenCode 2"), true);
    assert_requests(&obs, &["primary-token", "fallback-access"]);

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // Native default account retains its origin.
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
          bodyText: JSON.stringify(__test_state.successUsage)
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

    assert_eq!(first_request_auth(&obs), "token fallback-access-2");
    assert_eq!(result.outputs.len(), 1);
    assert_credits(&result, "opencode-1", "opencode", Some("OpenCode 2"), true);
    assert_requests(&obs, &["fallback-access-2"]);

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
            "~/.local/share/opencode/accounts.json",
            "~/.config/opencode/auth.json",
            "~/.config/opencode/accounts.json",
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
    assert_eq!(result.outputs.len(), 1);
    assert_eq!(result.outputs[0].account.id, "opencode-0");
    assert_requests(&obs, &[]);
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
          bodyText: JSON.stringify(__test_state.successUsage)
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

    // Token source: opencode auth has "github-copilot" key.
    assert_eq!(first_request_auth(&obs), "token opencode-access");
    assert_eq!(result.outputs.len(), 1);
    assert_credits(&result, "opencode-0", "opencode", Some("OpenCode 2"), true);
    assert_requests(&obs, &["opencode-access"]);

    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode account retains its source origin.
    assert_opencode_origin(&result, "opencode-0");
}

#[test]
fn copilot_override_only_default_soft_fail() {
    // Only one account — no suppression even if error policy is set.
    // HTTP 200 without quota data is a genuine native probe failure.
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
          bodyText: "{}"
        });
        "#,
        assertion: Some(ASSERTION_SCRIPT),
    });
    match outcome {
        ProviderOutcome::Probe { result, assertion } => {
            assert!(!result.outputs.is_empty(), "expected probe outputs");
            assert_eq!(result.outputs[0].provider_id, "copilot");
            let obs = assertion.expect("assertion should produce state");
            assert_eq!(result.outputs.len(), 1);
            match &result.outputs[0].lines[..] {
                [MetricLine::Badge { label, text, .. }] => {
                    assert_eq!(label, "Error");
                    assert_eq!(text, "Copilot usage data is unavailable for this account.");
                }
                other => panic!("expected visible native failure, got {other:?}"),
            }
            assert_requests(&obs, &["native-token"]);
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
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // Native default account retains its origin.
    assert_default_origin(&result);
    assert_eq!(result.outputs.len(), 1, "empty source creates no account");
    assert_requests(&obs, &[]);
}

#[test]
fn copilot_override_unreadable_source_creates_error_account() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = "throw-on-read";
        __test_state.readErrors["~/.local/share/opencode/auth.json"] = true;
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
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode account retains its source origin.
    assert_opencode_origin(&result, "opencode-0");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(result.outputs.len(), 1);
    assert_requests(&obs, &[]);
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
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // Native default account retains its origin.
    assert_default_origin(&result);
    assert_eq!(result.outputs.len(), 1, "missing provider is omitted");
    assert_requests(&obs, &[]);
}

#[test]
fn copilot_override_malformed_provider_block_null_error() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({ "github-copilot": null });
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
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode account retains its source origin.
    assert_opencode_origin(&result, "opencode-0");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(result.outputs.len(), 1);
    assert_requests(&obs, &[]);
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
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode account retains its source origin.
    assert_opencode_origin(&result, "opencode-0");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(result.outputs.len(), 1);
    assert_requests(&obs, &[]);
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
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode account retains its source origin.
    assert_opencode_origin(&result, "opencode-0");
    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(result.outputs.len(), 1);
    assert_requests(&obs, &[]);
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
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // Native default account retains its origin.
    assert_default_origin(&result);

    let error_text: String = result.outputs[0]
        .lines
        .iter()
        .map(|l| format!("{}", l))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(result.outputs.len(), 1);
    assert_requests(&obs, &[]);
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
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode account retains its source origin.
    assert_opencode_origin(&result, "opencode-0");
    assert_eq!(result.outputs.len(), 1, "native error is suppressed");
    assert_requests(&obs, &[]);
}

#[test]
fn copilot_override_no_cross_source_fallback() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.account = "opencode-0";
        __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({ token: "native" });
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({ "github-copilot": { type: "oauth", access: "oc-access", expires: 0 } });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify(__test_state.successUsage) }, { status: 200, headers: {}, bodyText: JSON.stringify(__test_state.successUsage) });
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
    assert!(!result.outputs.is_empty());
    assert_eq!(result.outputs[0].provider_id, "copilot");

    // OpenCode account retains its source origin.
    assert_opencode_origin(&result, "opencode-0");
    assert_eq!(result.outputs.len(), 2);
    assert_credits(&result, "default", "native", None, true);
    assert_credits(&result, "opencode-0", "opencode", Some("OpenCode 2"), true);
    assert_requests(&obs, &["native", "oc-access"]);
}

#[test]
fn copilot_override_two_valid_files_stable_ids() {
    let (result, obs) = run_copilot_probe(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({ "github-copilot": { type: "oauth", access: "token-0", expires: 0 } });
        __test_state.files["~/.config/opencode/auth.json"] = JSON.stringify({ "github-copilot": { type: "oauth", access: "token-1", expires: 0 } });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify(__test_state.successUsage) });
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify(__test_state.successUsage) });
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
    let accounts: Vec<String> = result
        .outputs
        .iter()
        .filter(|o| o.account.id.starts_with("opencode-"))
        .map(|o| o.account.id.clone())
        .collect();
    assert_eq!(result.outputs.len(), 2);
    assert_credits(&result, "opencode-0", "opencode", Some("OpenCode 2"), true);
    assert_credits(&result, "opencode-1", "opencode", Some("OpenCode 3"), true);
    assert_requests(&obs, &["token-0", "token-1"]);
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
        __test_state.responses.usage.push({ status: 200, headers: {}, bodyText: JSON.stringify(__test_state.successUsage) });
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
    let accounts: Vec<String> = result
        .outputs
        .iter()
        .filter(|o| o.account.id.starts_with("opencode-"))
        .map(|o| o.account.id.clone())
        .collect();
    assert_eq!(result.outputs.len(), 1);
    assert_credits(&result, "opencode-1", "opencode", Some("OpenCode 2"), true);
    assert_requests(&obs, &["second-token"]);
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
    use openusage_cli::plugin_engine::manifest::{LoadedPlugin, PluginManifest};
    use openusage_cli::plugin_engine::runtime::run_probe_in_sandbox;

    // The pinned plugin, native credential loading and normal HTTP wrapper run
    // unchanged; only the transport is fixture-only, with no network fallback.
    let plugin_script = format!(
        "{}{}",
        copilot_plugin_script(),
        r#"
        ;(function () {
          var ctx = globalThis.__openusage_ctx;
          var writeText = ctx.host.fs.writeText;
          var requests = [];
          var writes = [];
          function observe() {
            writeText(ctx.app.appDataDir + "/fixture-observed.json", JSON.stringify({requests: requests, writes: writes}));
          }
          ctx.host.fs.writeText = function (path, text) {
            writes.push(path);
            observe();
            return writeText(path, text);
          };
          ctx.host.http.request = function (opts) {
            requests.push({method: opts.method, url: opts.url, authorization: opts.headers.Authorization});
            observe();
            if (opts.method !== "GET" || opts.url !== "https://api.github.com/copilot_internal/user" ||
                ["token test-access-token", "token native-state-token"].indexOf(opts.headers.Authorization) === -1) {
              throw new Error("unexpected fixture request");
            }
            return {status: 200, headers: {}, bodyText: JSON.stringify({
              copilot_plan: "pro", quota_reset_date: "2099-01-15T00:00:00Z",
              quota_snapshots: {premium_interactions: {entitlement: 300, remaining: 225}}
            })};
          };
        })();
    "#
    );
    fs::write(
        paths.overrides.join("copilot.js"),
        copilot_override_script(),
    )
    .expect("write override");
    let opencode_dir = paths.root.join(".local/share/opencode");
    fs::create_dir_all(&opencode_dir).expect("create OpenCode directory");
    let auth_text =
        serde_json::json!({"github-copilot": {"type": "oauth", "access": "test-access-token"}})
            .to_string();
    let auth_path = opencode_dir.join("auth.json");
    fs::write(&auth_path, &auth_text).expect("write OpenCode auth");
    let native_path = paths.app_data.join("plugins_data/copilot/auth.json");
    let plugin = LoadedPlugin {
        manifest: PluginManifest {
            schema_version: 1,
            id: "copilot".into(),
            name: "Copilot".into(),
            version: "0.0.0".into(),
            entry: "plugin.js".into(),
            icon: "icon.svg".into(),
            brand_color: None,
            lines: vec![],
            links: vec![],
        },
        plugin_dir: repo_root().join("vendor/crossusage/plugins/copilot"),
        entry_script: plugin_script,
        icon_data_url: "data:image/svg+xml;base64,".into(),
    };

    for native_present in [false, true] {
        if native_present {
            fs::create_dir_all(native_path.parent().unwrap())
                .expect("create native state directory");
            fs::write(&native_path, r#"{"token":"native-state-token"}"#)
                .expect("write native state");
        }
        let result = run_probe_in_sandbox(
            &plugin,
            &paths.app_data,
            "test",
            Some(&paths.overrides),
            &paths.root,
        );
        assert_eq!(result.outputs.len(), if native_present { 2 } else { 1 });
        assert_credits(&result, "opencode-0", "opencode", Some("OpenCode 2"), true);
        if native_present {
            assert_credits(&result, "default", "native", None, true);
        } else {
            assert!(
                result.outputs.iter().all(|o| o.account.id != "default"),
                "native error suppressed when OpenCode exists"
            );
        }
        let observed: Value = serde_json::from_str(
            &fs::read_to_string(paths.app_data.join("fixture-observed.json"))
                .expect("transport observation"),
        )
        .unwrap();
        let tokens = if native_present {
            vec!["native-state-token", "test-access-token"]
        } else {
            vec!["test-access-token"]
        };
        assert_requests(&observed, &tokens);
        assert_eq!(
            fs::read_to_string(&auth_path).unwrap(),
            auth_text,
            "OpenCode credentials unchanged"
        );
        assert_eq!(native_path.exists(), native_present, "no native writeback");
        if native_present {
            assert_eq!(
                fs::read_to_string(&native_path).unwrap(),
                r#"{"token":"native-state-token"}"#
            );
        }
        assert_eq!(result.subscriptions.len(), 5);
        for candidate in [
            ".local/share/opencode/auth.json",
            ".local/share/opencode/accounts.json",
            ".config/opencode/auth.json",
            ".config/opencode/accounts.json",
        ] {
            let path = paths.root.join(candidate);
            let sub = result
                .subscriptions
                .iter()
                .find(|s| s.path == path)
                .expect("candidate subscription collected by production lifecycle");
            assert_eq!(sub.existed_at_declaration, path.exists());
        }
    }

    let mut observed_plugin = plugin;
    observed_plugin.entry_script.push_str(
        r#"
        ;(function () {
          var probe = globalThis.__openusage_plugin.probe;
          globalThis.__openusage_plugin.probe = function (ctx) {
            var output = probe(ctx);
            ctx.host.fs.writeText(ctx.app.appDataDir + "/late-write.txt", "fixture write after response");
            return output;
          };
        })();
        "#,
    );
    let result = run_probe_in_sandbox(
        &observed_plugin,
        &paths.app_data,
        "test",
        Some(&paths.overrides),
        &paths.root,
    );
    assert_eq!(result.outputs.len(), 2);
    assert_credits(&result, "default", "native", None, true);
    assert_credits(&result, "opencode-0", "opencode", Some("OpenCode 2"), true);
    let observed: Value = serde_json::from_str(
        &fs::read_to_string(paths.app_data.join("fixture-observed.json"))
            .expect("complete write observation"),
    )
    .unwrap();
    let write_path = paths.app_data.join("late-write.txt");
    assert_eq!(
        observed["writes"],
        serde_json::json!([write_path, write_path]),
        "writes after the last HTTP response must remain observable"
    );
}

// ═══════════════════════════════════════════════════════════════════════

/// Harness for Copilot tests — sets up mock host with keychain support.
const HARNESS_SCRIPT: &str = r#"
(function () {
  globalThis.__test_state = {
    files: {},
    keychain: {},
    requests: [],
    writes: [],
    privacy: null,
    successUsage: {
      copilot_plan: "pro",
      quota_reset_date: "2099-01-15T00:00:00Z",
      quota_snapshots: {premium_interactions: {entitlement: 300, remaining: 225}}
    },
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
          globalThis.__test_state.writes.push({kind: "file", path: path});
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
          globalThis.__test_state.writes.push({kind: "keychain", service: service});
          globalThis.__test_state.keychain[service] = String(value);
        },
        deleteGenericPassword: function (service) {
          globalThis.__test_state.writes.push({kind: "delete", service: service});
          delete globalThis.__test_state.keychain[service];
        }
      },
      http: {
        request: function (opts) {
          opts = opts || {};
          if (__test_state.inspectPrivacy && __test_state.privacy === null) {
            var privateNames = ["ROUTE_REGISTRY", "OPENCODE_SOURCES", "COPILOT_PROVIDER_KEY",
              "isNonEmptyString", "isPlainObject", "logWarn", "parseJsonLoose", "parseCopilotCredential", "discoverAccounts", "sj"];
            __test_state.privacy = privateNames.map(function (name) {
              var visibility = (0, eval)("typeof " + name);
              globalThis[name] = function () { throw new Error("poisoned private global: " + name); };
              return {name: name, visibility: visibility};
            });
            globalThis.ROUTE_REGISTRY = {"opencode-0": {token: "poisoned-token"}};
            globalThis.OPENCODE_SOURCES = [];
          }
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
      planLabel: function (value) { return value === "pro" ? "Pro" : String(value || ""); }
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
    writes: state.writes,
    privacy: state.privacy,
    logs: state.logs
  });
})();
"#;
