#[path = "support/credential_fixture.rs"]
mod credential_fixture;
#[path = "support/hermetic.rs"]
mod hermetic;

use credential_fixture::{AUTH, PI_AUTH, auth, native, pi_auth, probe, write};
use serde_json::{Value, json};
use std::fs;

const PROVIDERS: [(&str, &str); 4] = [
    ("claude", "anthropic"),
    ("codex", "openai-codex"),
    ("copilot", "github-copilot"),
    ("openrouter", "openrouter"),
];

fn line<'a>(output: &'a Value, label: &str) -> &'a Value {
    output["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["label"] == label)
        .unwrap_or_else(|| panic!("missing {label}: {output}"))
}

fn pi(outputs: &[Value]) -> &Value {
    let output = outputs
        .iter()
        .find(|output| output["account"]["origin"] == "pi")
        .unwrap_or_else(|| panic!("missing Pi account: {outputs:?}"));
    assert_eq!(output["account"]["id"], "pi-0");
    assert_eq!(output["account"]["name"], "Pi");
    output
}

fn error(output: &Value) -> &str {
    line(output, "Error")["text"].as_str().unwrap()
}

#[test]
fn pi_discovery_is_separate_from_native_and_opencode_on_all_four_providers() {
    let Some(paths) =
        hermetic::enter("pi_discovery_is_separate_from_native_and_opencode_on_all_four_providers")
    else {
        return;
    };
    native(&paths);
    write(&paths, AUTH, &auth().to_string());
    write(&paths, PI_AUTH, &pi_auth().to_string());
    for (id, _) in PROVIDERS {
        let (outputs, observed) = probe(&paths, id, json!({}));
        assert_eq!(outputs.len(), 3, "{id}: {outputs:?}");
        assert_eq!(
            outputs
                .iter()
                .map(|o| o["account"]["origin"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["native", "opencode", "pi"]
        );
        let output = pi(&outputs);
        let label = match id {
            "openrouter" => "Balance",
            "copilot" => "Credits",
            _ => "Session",
        };
        if id == "openrouter" {
            assert_eq!(line(output, label)["value"], "$4");
        } else {
            assert_eq!(line(output, label)["used"], 63.0);
        }
        let requests = observed["requests"].as_array().unwrap();
        assert_eq!(
            requests.len(),
            if id == "copilot" { 3 } else { 6 },
            "{id}: {requests:?}"
        );
        let token = match id {
            "claude" => "Bearer pi-claude",
            "codex" => "Bearer pi-codex",
            "copilot" => "token pi-copilot",
            _ => "Bearer pi-router",
        };
        let last = if id == "copilot" { 1 } else { 2 };
        for request in &requests[requests.len() - last..] {
            assert_eq!(request["headers"]["Authorization"], token);
            if id == "codex" {
                assert_eq!(request["headers"]["ChatGPT-Account-Id"], "pi-account");
            }
        }
        assert_eq!(observed["writes"], json!([]));
        assert!(
            observed["subscriptions"]
                .as_array()
                .unwrap()
                .contains(&json!(format!("~/{PI_AUTH}")))
        );
        assert!(
            observed["collectedSubscriptions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["path"] == json!(paths.root.join(PI_AUTH)) && s["existed"] == true)
        );
        if ["claude", "codex"].contains(&id) {
            assert_eq!(
                observed["localUsage"].as_array().unwrap().len(),
                if id == "claude" { 1 } else { 2 }
            );
            assert!(!output["lines"].as_array().unwrap().iter().any(|line| {
                ["Today", "Last 30 Days"].contains(&line["label"].as_str().unwrap_or_default())
            }));
        }
    }
}

#[test]
fn pi_missing_empty_and_wrong_provider_keys_do_not_add_accounts() {
    let Some(paths) =
        hermetic::enter("pi_missing_empty_and_wrong_provider_keys_do_not_add_accounts")
    else {
        return;
    };
    native(&paths);
    for contents in [
        None,
        Some(""),
        Some(" \n\t"),
        Some("{}"),
        Some(
            r#"{"openai":{"type":"oauth","access":"modern","refresh":"modern","expires":4102444800000}}"#,
        ),
    ] {
        if let Some(contents) = contents {
            write(&paths, PI_AUTH, contents);
        } else {
            let _ = fs::remove_file(paths.root.join(PI_AUTH));
        }
        for (id, _) in PROVIDERS {
            let (outputs, observed) = probe(&paths, id, json!({}));
            assert_eq!(outputs.len(), 1, "{id}: {outputs:?}");
            assert_eq!(outputs[0]["account"]["origin"], "native");
            assert!(
                observed["subscriptions"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(format!("~/{PI_AUTH}")))
            );
        }
    }
}

#[test]
fn pi_invalid_documents_and_credentials_are_account_scoped() {
    let Some(paths) = hermetic::enter("pi_invalid_documents_and_credentials_are_account_scoped")
    else {
        return;
    };
    native(&paths);
    write(&paths, AUTH, &auth().to_string());
    for contents in ["{bad", "null", "[]", "42"] {
        write(&paths, PI_AUTH, contents);
        for (id, _) in PROVIDERS {
            let (outputs, observed) = probe(&paths, id, json!({}));
            assert_eq!(outputs.len(), 3, "{id}: {outputs:?}");
            assert!(!error(pi(&outputs)).is_empty());
            assert_eq!(
                observed["requests"].as_array().unwrap().len(),
                if id == "copilot" { 2 } else { 4 }
            );
        }
    }
    for (id, key) in PROVIDERS {
        for entry in [
            Value::Null,
            json!({}),
            json!({"type":"api", "key":"wrong-format"}),
            json!({"type":"oauth", "access":"access", "refresh":"", "expires":"invalid"}),
        ] {
            let mut doc = pi_auth();
            doc[key] = entry;
            write(&paths, PI_AUTH, &doc.to_string());
            let (outputs, observed) = probe(&paths, id, json!({}));
            assert!(!error(pi(&outputs)).is_empty());
            assert_eq!(observed["writes"], json!([]));
        }
    }
}

#[test]
fn pi_copilot_uses_github_refresh_not_inference_access_and_rejects_enterprise() {
    let Some(paths) = hermetic::enter(
        "pi_copilot_uses_github_refresh_not_inference_access_and_rejects_enterprise",
    ) else {
        return;
    };
    let mut doc = pi_auth();
    write(&paths, PI_AUTH, &doc.to_string());
    let (outputs, observed) = probe(&paths, "copilot", json!({}));
    assert_eq!(outputs.len(), 1);
    assert_eq!(line(pi(&outputs), "Credits")["used"], 63.0);
    assert_eq!(
        observed["requests"][0]["headers"]["Authorization"],
        "token pi-copilot"
    );
    assert_eq!(observed["writes"], json!([]));
    doc["github-copilot"]["enterpriseUrl"] = json!("enterprise.example");
    write(&paths, PI_AUTH, &doc.to_string());
    let (outputs, observed) = probe(&paths, "copilot", json!({}));
    assert!(error(pi(&outputs)).contains("Enterprise"));
    assert_eq!(
        observed["requests"],
        json!([]),
        "enterprise credentials must not go to public GitHub"
    );
}

#[test]
fn pi_openrouter_accepts_literal_api_keys_and_permanent_oauth_keys_only() {
    let Some(paths) =
        hermetic::enter("pi_openrouter_accepts_literal_api_keys_and_permanent_oauth_keys_only")
    else {
        return;
    };
    for entry in [
        json!({"type":"api_key","key":"pi-router"}),
        json!({"type":"oauth","access":"pi-router","refresh":"","expires":9007199254740991_u64}),
    ] {
        write(&paths, PI_AUTH, &json!({"openrouter":entry}).to_string());
        let (outputs, observed) = probe(&paths, "openrouter", json!({}));
        assert_eq!(outputs.len(), 1);
        assert_eq!(line(pi(&outputs), "Balance")["value"], "$4");
        assert_eq!(observed["requests"].as_array().unwrap().len(), 2);
        assert_eq!(observed["writes"], json!([]));
    }
    for key in ["!echo secret", "$OPENROUTER_API_KEY", "${KEY}", ""] {
        write(
            &paths,
            PI_AUTH,
            &json!({"openrouter":{"type":"api_key","key":key}}).to_string(),
        );
        let (outputs, observed) = probe(&paths, "openrouter", json!({}));
        assert!(!error(pi(&outputs)).is_empty());
        assert_eq!(
            observed["requests"],
            json!([]),
            "expressions are not credentials or executable input"
        );
    }
}

#[test]
fn pi_oauth_refresh_is_locked_and_preserves_other_sources_and_metadata() {
    let Some(paths) =
        hermetic::enter("pi_oauth_refresh_is_locked_and_preserves_other_sources_and_metadata")
    else {
        return;
    };
    native(&paths);
    let oc = auth();
    write(&paths, AUTH, &oc.to_string());
    let oc_bytes = fs::read(paths.root.join(AUTH)).unwrap();
    for (id, key) in [("claude", "anthropic"), ("codex", "openai-codex")] {
        for (reactive, no_rotation) in [(false, false), (true, false), (false, true)] {
            let mut doc = pi_auth();
            doc[key]["extra"] = json!({"preserved":true});
            if !reactive {
                doc[key]["expires"] = json!(0);
            }
            write(&paths, PI_AUTH, &doc.to_string());
            let before = chrono::Utc::now().timestamp_millis();
            let (outputs, observed) = probe(
                &paths,
                id,
                json!({"prefix":"pi","authPath":format!("~/{PI_AUTH}"),"providerKey":key,"retryPi":reactive,"noRotation":no_rotation,"changeDuringRefresh":true}),
            );
            assert_eq!(line(pi(&outputs), "Session")["used"], 63.0);
            let saved: Value =
                serde_json::from_slice(&fs::read(paths.root.join(PI_AUTH)).unwrap()).unwrap();
            assert_eq!(saved[key]["access"], "pi-renewed");
            assert_eq!(
                saved[key]["refresh"],
                if no_rotation {
                    doc[key]["refresh"].as_str().unwrap()
                } else {
                    "pi-rotated"
                }
            );
            assert_eq!(saved[key]["extra"], doc[key]["extra"]);
            let expires = saved[key]["expires"].as_i64().unwrap();
            let renewal_delay = if id == "claude" { 3_300_000 } else { 3_600_000 };
            assert!(
                expires >= before + renewal_delay
                    && expires <= chrono::Utc::now().timestamp_millis() + renewal_delay
            );
            assert_eq!(saved["concurrentSibling"]["key"], "preserve-latest");
            for other in doc
                .as_object()
                .unwrap()
                .keys()
                .filter(|other| *other != key)
            {
                assert_eq!(saved[other], doc[other]);
            }
            assert_eq!(fs::read(paths.root.join(AUTH)).unwrap(), oc_bytes);
            assert_eq!(
                observed["locks"],
                json!([{"path":format!("~/{PI_AUTH}"),"held":true}])
            );
            assert!(!paths.root.join(format!("{PI_AUTH}.lock")).exists());
            assert_eq!(observed["writes"].as_array().unwrap().len(), 1);
            assert_eq!(observed["writes"][0]["path"], format!("~/{PI_AUTH}"));
            let refresh = observed["requests"]
                .as_array()
                .unwrap()
                .iter()
                .find(|request| request["method"] == "POST")
                .unwrap();
            let body = refresh["bodyText"].as_str().unwrap();
            assert!(body.contains(doc[key]["refresh"].as_str().unwrap()));
            if id == "codex" {
                assert!(body.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
            }
        }
    }
}

#[test]
fn pi_refresh_failures_are_visible_and_release_the_shared_lock() {
    let Some(paths) =
        hermetic::enter("pi_refresh_failures_are_visible_and_release_the_shared_lock")
    else {
        return;
    };
    for (id, key) in [("claude", "anthropic"), ("codex", "openai-codex")] {
        for failure in [
            "writeFailure",
            "corruptDuringRefresh",
            "replaceLoginDuringRefresh",
            "invalidExpiry",
        ] {
            let mut doc = pi_auth();
            doc[key]["expires"] = json!(0);
            write(&paths, PI_AUTH, &doc.to_string());
            let before = fs::read(paths.root.join(PI_AUTH)).unwrap();
            let mut options =
                json!({"prefix":"pi","authPath":format!("~/{PI_AUTH}"),"providerKey":key});
            options[failure] = json!(true);
            let (outputs, observed) = probe(&paths, id, options);
            assert!(
                !error(pi(&outputs)).is_empty(),
                "{id}/{failure}: {outputs:?}"
            );
            assert!(!paths.root.join(format!("{PI_AUTH}.lock")).exists());
            assert_eq!(
                observed["locks"],
                json!([{"path":format!("~/{PI_AUTH}"),"held":true}])
            );
            assert_eq!(
                observed["requests"].as_array().unwrap().len(),
                1,
                "no usage after failed refresh"
            );
            if ["writeFailure", "invalidExpiry"].contains(&failure) {
                assert_eq!(fs::read(paths.root.join(PI_AUTH)).unwrap(), before);
            }
            if failure == "replaceLoginDuringRefresh" {
                let saved: Value =
                    serde_json::from_slice(&fs::read(paths.root.join(PI_AUTH)).unwrap()).unwrap();
                assert_eq!(saved[key]["access"], "different-login");
                assert_eq!(observed["writes"], json!([]));
            }
        }
    }
}

#[test]
fn pi_refresh_rereads_after_lock_and_skips_only_already_fresh_credentials() {
    let Some(paths) =
        hermetic::enter("pi_refresh_rereads_after_lock_and_skips_only_already_fresh_credentials")
    else {
        return;
    };
    for (id, key) in [("claude", "anthropic"), ("codex", "openai-codex")] {
        for (reactive, updated_expired) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut doc = pi_auth();
            if !reactive {
                doc[key]["expires"] = json!(0);
            }
            write(&paths, PI_AUTH, &doc.to_string());
            let (outputs, observed) = probe(
                &paths,
                id,
                json!({"prefix":"pi","authPath":format!("~/{PI_AUTH}"),"providerKey":key,"retryPi":reactive,"updateBeforeLock":true,"updatedExpired":updated_expired}),
            );
            assert_eq!(line(pi(&outputs), "Session")["used"], 63.0);
            let posts: Vec<_> = observed["requests"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "POST")
                .collect();
            assert_eq!(
                posts.len(),
                usize::from(updated_expired),
                "{id}/reactive={reactive}/expired={updated_expired}: {observed}"
            );
            if updated_expired {
                assert!(
                    posts[0]["bodyText"]
                        .as_str()
                        .unwrap()
                        .contains("pi-latest-refresh")
                );
            }
            let saved: Value =
                serde_json::from_slice(&fs::read(paths.root.join(PI_AUTH)).unwrap()).unwrap();
            assert_eq!(
                saved[key]["access"],
                if updated_expired {
                    "pi-renewed"
                } else {
                    "pi-latest"
                }
            );
            assert_eq!(
                observed["writes"].as_array().unwrap().len(),
                usize::from(updated_expired)
            );
            assert!(!paths.root.join(format!("{PI_AUTH}.lock")).exists());
        }
    }
}

#[test]
fn pi_authentication_failures_never_retry_with_native_or_opencode() {
    let Some(paths) =
        hermetic::enter("pi_authentication_failures_never_retry_with_native_or_opencode")
    else {
        return;
    };
    native(&paths);
    write(&paths, AUTH, &auth().to_string());
    for (id, key) in PROVIDERS {
        write(&paths, PI_AUTH, &pi_auth().to_string());
        let (outputs, observed) = probe(
            &paths,
            id,
            json!({"prefix":"pi","authPath":format!("~/{PI_AUTH}"),"providerKey":key,"rejectPi":true}),
        );
        assert_eq!(outputs.len(), 3);
        assert!(!error(pi(&outputs)).is_empty());
        let requests = observed["requests"].as_array().unwrap();
        let offset = if id == "copilot" { 2 } else { 4 };
        for request in &requests[offset..] {
            if request["method"] == "POST" {
                assert!(request["bodyText"].as_str().unwrap().contains("pi-"));
            } else {
                assert!(
                    request["headers"]["Authorization"]
                        .as_str()
                        .unwrap()
                        .contains("pi-")
                );
            }
        }
    }
}

#[test]
fn pi_subscription_survives_the_daemon_discovery_lifecycle_with_probes_disabled() {
    let Some(paths) = hermetic::enter(
        "pi_subscription_survives_the_daemon_discovery_lifecycle_with_probes_disabled",
    ) else {
        return;
    };
    use openusage_cli::{
        daemon::DaemonState, plugin_engine::manifest, restart_watcher::ProviderWatchCommand,
    };
    use std::{path::Path, time::Duration};
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let plugins: Vec<_> = manifest::load_plugins_from_dir(&root.join("vendor/crossusage/plugins"))
        .into_iter()
        .filter(|plugin| PROVIDERS.iter().any(|(id, _)| *id == plugin.manifest.id))
        .collect();
    assert_eq!(plugins.len(), 4);
    for plugin in &plugins {
        let file = format!("{}.js", plugin.manifest.id);
        fs::copy(
            root.join("plugin-overrides").join(&file),
            paths.overrides.join(file),
        )
        .unwrap();
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let state = DaemonState::new(
            plugins,
            paths.app_data.clone(),
            "test".into(),
            Some(paths.overrides.clone()),
            Some(tx),
        )
        .with_enabled_auth_sources(serde_yaml::from_str("default: []").unwrap());
        let path = paths.root.join(PI_AUTH);
        let monitor = tokio::spawn(async move {
            for exists in [false, true, false] {
                for _ in 0..4 {
                    let ProviderWatchCommand::ClearProvider { provider_id, ack } =
                        rx.recv().await.unwrap()
                    else {
                        panic!("clear before discovery");
                    };
                    ack.send(()).unwrap();
                    let ProviderWatchCommand::ReplaceProvider {
                        provider_id: replaced,
                        files,
                        ack,
                    } = rx.recv().await.unwrap()
                    else {
                        panic!("replace after discovery");
                    };
                    assert_eq!(replaced, provider_id);
                    assert_eq!(
                        files.len(),
                        if ["codex", "copilot"].contains(&provider_id.as_str()) {
                            5
                        } else {
                            2
                        }
                    );
                    let pi = files
                        .iter()
                        .find(|file| file.path == path)
                        .expect("Pi subscription collected even without account probes");
                    assert_eq!(pi.existed_at_declaration, exists);
                    ack.send(()).unwrap();
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            for exists in [false, true, false] {
                if exists {
                    write(&paths, PI_AUTH, &pi_auth().to_string());
                } else {
                    let _ = fs::remove_file(paths.root.join(PI_AUTH));
                }
                assert!(state.refresh(None).await.unwrap().is_empty());
                assert!(state.cached(None).await.is_empty());
            }
            monitor.await.unwrap();
        })
        .await
        .expect("watch acknowledgements complete");
    });
}
