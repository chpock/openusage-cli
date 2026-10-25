use super::*;
pub(super) use crate::hermetic;

#[test]
fn native_logs_missing_and_invalid_options() {
    let Some(paths) =
        hermetic::enter("plugin_engine::analytics::tests::native_logs_missing_and_invalid_options")
    else {
        return;
    };
    let policy = FilesystemPolicy::sandbox(&paths.root).unwrap();
    for source in [Source::Claude, Source::Codex, Source::Grok, Source::Cursor] {
        let result = query(source, "{\"since\":\"20260101\"}", &policy);
        assert_eq!(result["status"], "no_data", "{result}");
        for opts in [
            "[]",
            "{\"since\":7}",
            "{\"since\":\"20260230\"}",
            "{\"homePath\":3}",
        ] {
            assert_eq!(query(source, opts, &policy)["status"], "error");
        }
    }
    assert_eq!(query(Source::Antigravity, "{}", &policy)["status"], "error");
    assert_eq!(
        sdk(&paths, "ctx.host.claudeLogs.queryDaily({since:7})")["status"],
        "error"
    );
    assert_eq!(
        sdk(&paths, "ctx.host.antigravityLogs.queryDaily({})")["error"],
        "local-log SQLite bridge is disabled in sandbox"
    );
    put(&paths.root, ".cursor/projects", "not a directory");
    assert_eq!(
        sdk(&paths, "ctx.host.cursorLogs.queryDaily({})")["status"],
        "error",
        "reachable directory errors must not become no_data"
    );
}

use crate::plugin_engine::{manifest, runtime};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(super) fn put(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    path
}

fn claude(id: &str, timestamp: &str, input: i64, cost: Option<f64>) -> Value {
    let mut row = json!({"timestamp":timestamp,"requestId":format!("r-{id}"),"version":"1.0.0",
        "message":{"id":id,"model":"claude-sonnet-4-20250514","usage":{
            "input_tokens":input,"output_tokens":50,"cache_creation_input_tokens":20,"cache_read_input_tokens":30}}});
    if let Some(cost) = cost {
        row["costUSD"] = json!(cost);
    }
    row
}

fn pi(id: &str, provider: &str, timestamp: &str, cost: f64) -> Value {
    json!({"type":"message","id":id,"timestamp":timestamp,"message":{
        "role":"assistant","provider":provider,"model":"claude-sonnet-4-20250514",
        "usage":{"input":10,"output":5,"cacheRead":1,"cacheWrite":2,"totalTokens":18,"cost":{"total":cost}}}})
}

fn codex(timestamp: &str) -> Value {
    json!({"type":"event_msg","timestamp":timestamp,"payload":{"type":"token_count","model":"gpt-5.6-sol",
        "info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":10,"total_tokens":140}}}})
}

fn grok(id: &str, timestamp: &str, ticks: f64) -> Value {
    json!({"timestamp":timestamp,"params":{"_meta":{"eventId":id},"update":{"sessionUpdate":"turn_completed",
        "usage":{"costUsdTicks":ticks,"modelUsage":{"grok-test":{"inputTokens":100,"cachedReadTokens":20,"cacheCreationTokens":10,"outputTokens":30}}}}}})
}

fn sdk(paths: &hermetic::HermeticPaths, body: &str) -> Value {
    let plugin = manifest::LoadedPlugin {
        manifest: manifest::PluginManifest {
            schema_version: 1,
            id: "local-log-fixture".into(),
            name: "Fixture".into(),
            version: "0".into(),
            entry: "plugin.js".into(),
            icon: "icon.svg".into(),
            brand_color: None,
            lines: vec![],
            links: vec![],
        },
        plugin_dir: paths.root.clone(),
        icon_data_url: String::new(),
        entry_script: format!(
            "globalThis.__openusage_plugin={{probe(ctx){{let result=({body});return {{lines:[ctx.line.text({{label:'Result',value:JSON.stringify(result)}})]}};}}}}"
        ),
    };
    let result = runtime::run_probe_in_sandbox(
        &plugin,
        &paths.app_data,
        "test",
        Some(&paths.overrides),
        &paths.root,
    );
    match &result.outputs[0].lines[..] {
        [runtime::MetricLine::Text { label, value, .. }] if label == "Result" => {
            serde_json::from_str(value).unwrap()
        }
        lines => panic!("SDK fixture failed: {lines:?}"),
    }
}

pub(super) fn events() -> Vec<(&'static str, PathBuf)> {
    scan::EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()))
}
pub(super) fn paths_for(events: &[(&str, PathBuf)], kind: &str) -> Vec<PathBuf> {
    events
        .iter()
        .filter(|e| e.0 == kind)
        .map(|e| e.1.clone())
        .collect()
}
fn close(actual: &Value, expected: f64) {
    assert!(
        (actual.as_f64().unwrap() - expected).abs() < 1e-10,
        "actual {actual}, expected {expected}"
    );
}

#[test]
fn native_logs_populated_sdk_and_exact_cache_reuse() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::analytics::tests::native_logs_populated_sdk_and_exact_cache_reuse",
    ) else {
        return;
    };
    let timestamp = "2026-10-24T10:00:00Z";
    let a = put(
        &paths.root,
        ".claude/projects/demo/a.jsonl",
        &format!("{}\n{{broken\n", claude("a", timestamp, 100, None)),
    );
    let b = put(
        &paths.root,
        ".claude/projects/demo/b.jsonl",
        &format!("{}\n{{\"truncated\":", claude("a", timestamp, 100, None)),
    );
    let pi_path = put(
        &paths.root,
        ".pi/agent/sessions/pi.jsonl",
        &format!(
            "{}\n{}\n{}\n",
            pi("p", "anthropic", timestamp, 0.25),
            pi("pc", "openai-codex", timestamp, 0.1),
            pi("p", "anthropic", timestamp, 0.25)
        ),
    );
    let call = "ctx.host.claudeLogs.queryDaily({since:'20261001'})";
    let first = sdk(&paths, call);
    assert_eq!(first["status"], "ok");
    let row = &first["data"]["daily"][0];
    assert_eq!(row["totalTokens"], 218);
    assert_eq!(row["inputTokens"], 110);
    assert_eq!(row["outputTokens"], 55);
    assert_eq!(row["cacheCreationTokens"], 22);
    assert_eq!(row["cacheReadTokens"], 31);
    close(&row["totalCost"], 0.251134);
    assert_eq!(row["costUSD"], Value::Null); // Pinned camelCase field is costUsd.
    close(&row["costUsd"], 0.251134);
    let first_events = events();
    assert_eq!(
        paths_for(&first_events, "parsed"),
        vec![a.clone(), b.clone(), pi_path.clone()]
    );
    assert_eq!(paths_for(&first_events, "read").len(), 3);
    assert_eq!(
        paths_for(&first_events, "aggregated"),
        vec![PathBuf::from("claude"), PathBuf::from("pi")]
    );
    assert_eq!(sdk(&paths, call), first);
    let repeat = events();
    assert!(paths_for(&repeat, "parsed").is_empty());
    assert!(paths_for(&repeat, "read").is_empty());
    assert_eq!(
        paths_for(&repeat, "reused"),
        vec![a.clone(), b.clone(), pi_path.clone()]
    );
    assert_eq!(paths_for(&repeat, "aggregated").len(), 2);
    fs::write(
        &b,
        format!("{}\n", claude("changed-b", timestamp, 200, Some(0.0))),
    )
    .unwrap();
    let changed = sdk(&paths, call);
    assert_eq!(changed["data"]["daily"][0]["totalTokens"], 518);
    close(&changed["data"]["daily"][0]["totalCost"], 0.251134);
    let change_events = events();
    assert_eq!(paths_for(&change_events, "parsed"), vec![b.clone()]);
    assert_eq!(paths_for(&change_events, "read"), vec![b]);
    assert_eq!(paths_for(&change_events, "reused"), vec![a, pi_path]);
    assert_eq!(paths_for(&change_events, "aggregated").len(), 2);
    println!(
        "cache evidence: first read/parse=3 reused=0 aggregate=2; repeat read/parse=0 reused=3 aggregate=2; change read/parse=1 reused=2 aggregate=2"
    );

    put(
        &paths.root,
        ".codex/sessions/a.jsonl",
        &format!("{}\n{}\n", codex(timestamp), codex(timestamp)),
    );
    put(
        &paths.root,
        ".codex/archived_sessions/a.jsonl",
        &format!("{}\n", codex("2026-10-24T11:00:00Z")),
    );
    let result = sdk(
        &paths,
        "ctx.host.codexLogs.queryDaily({since:'2026-10-01'})",
    );
    let row = &result["data"]["daily"][0];
    assert_eq!(row["totalTokens"], 158);
    assert_eq!(row["inputTokens"], 110);
    assert_eq!(row["outputTokens"], 45);
    close(&row["totalCost"], 0.101128);
    assert_eq!(row["models"]["gpt-5.6-sol"]["totalTokens"], 140);
    let codex_path = paths.root.join(".codex/sessions/a.jsonl");
    let initial = events();
    assert_eq!(paths_for(&initial, "parsed"), vec![codex_path.clone()]);
    assert_eq!(
        sdk(&paths, "ctx.host.codexLogs.queryDaily({since:'20261001'})"),
        result
    );
    let repeat = events();
    assert_eq!(
        paths_for(&repeat, "reused"),
        vec![
            codex_path.clone(),
            paths.root.join(".pi/agent/sessions/pi.jsonl")
        ]
    );
    assert!(paths_for(&repeat, "read").is_empty());
    assert!(paths_for(&repeat, "parsed").is_empty());
    assert_eq!(paths_for(&repeat, "aggregated").len(), 2);
    let mut changed_codex = codex(timestamp);
    changed_codex["payload"]["info"]["total_token_usage"]["input_tokens"] = json!(200);
    changed_codex["payload"]["info"]["total_token_usage"]["total_tokens"] = json!(240);
    fs::write(&codex_path, format!("{changed_codex}\n")).unwrap();
    let changed = sdk(&paths, "ctx.host.codexLogs.queryDaily({since:'20261001'})");
    assert_eq!(changed["data"]["daily"][0]["totalTokens"], 258);
    close(&changed["data"]["daily"][0]["totalCost"], 0.101528);
    let changed_events = events();
    assert_eq!(paths_for(&changed_events, "read"), vec![codex_path.clone()]);
    assert_eq!(paths_for(&changed_events, "parsed"), vec![codex_path]);
    assert_eq!(
        paths_for(&changed_events, "reused"),
        vec![paths.root.join(".pi/agent/sessions/pi.jsonl")]
    );
    assert_eq!(paths_for(&changed_events, "aggregated").len(), 2);

    put(
        &paths.root,
        ".grok/sessions/demo/updates.jsonl",
        &format!(
            "{}\n{}\n{}\n{{truncated",
            grok("g", timestamp, 2_000_000_000.0),
            grok("g", timestamp, 2_000_000_000.0),
            grok("zero", timestamp, 0.0)
        ),
    );
    let result = sdk(&paths, "ctx.host.grokLogs.queryDaily({since:'20261001'})");
    let row = &result["data"]["daily"][0];
    assert_eq!(row["totalTokens"], 260);
    assert_eq!(row["inputTokens"], 140);
    assert_eq!(row["cacheCreationTokens"], 20);
    assert_eq!(row["cacheReadTokens"], 40);
    close(&row["totalCost"], 0.2);
    let grok_path = paths.root.join(".grok/sessions/demo/updates.jsonl");
    assert_eq!(paths_for(&events(), "parsed"), vec![grok_path.clone()]);
    assert_eq!(
        sdk(&paths, "ctx.host.grokLogs.queryDaily({since:'20261001'})"),
        result
    );
    let repeat = events();
    assert_eq!(paths_for(&repeat, "reused"), vec![grok_path.clone()]);
    assert!(paths_for(&repeat, "read").is_empty());
    assert!(paths_for(&repeat, "parsed").is_empty());
    assert_eq!(paths_for(&repeat, "aggregated").len(), 1);
    fs::write(&grok_path, format!("{}\n", grok("changed", timestamp, 0.0))).unwrap();
    assert_eq!(
        sdk(&paths, "ctx.host.grokLogs.queryDaily({since:'20261001'})")["data"]["daily"][0]["totalTokens"],
        130
    );
    let changed = events();
    assert_eq!(paths_for(&changed, "parsed"), vec![grok_path.clone()]);
    assert_eq!(paths_for(&changed, "read"), vec![grok_path]);
    assert!(paths_for(&changed, "reused").is_empty());
    assert_eq!(paths_for(&changed, "aggregated").len(), 1);

    put(
        &paths.root,
        ".cursor/projects/demo/agent-transcripts/a.jsonl",
        "{\"message\":{\"content\":[{\"text\":\"éééééééé\"},{\"text\":\"abcdefgh\"}]}}\n{broken\n",
    );
    put(
        &paths.root,
        ".cursor/projects/demo/agent-transcripts/b.jsonl",
        "{\"message\":{\"content\":[{\"text\":\"abcd\"}]}}\n",
    );
    events();
    let result = sdk(&paths, "ctx.host.cursorLogs.queryDaily({since:'20261001'})");
    assert_eq!(result["data"]["daily"][0]["totalTokens"], 5);
    assert_eq!(result["data"]["daily"][0]["estimated"], true);
    assert_eq!(paths_for(&events(), "parsed").len(), 2);
    assert_eq!(
        sdk(&paths, "ctx.host.cursorLogs.queryDaily({since:'20261001'})"),
        result
    );
    let repeat = events();
    assert_eq!(paths_for(&repeat, "reused").len(), 2);
    assert!(paths_for(&repeat, "parsed").is_empty());
    let changed_path = put(
        &paths.root,
        ".cursor/projects/demo/agent-transcripts/a.jsonl",
        "{\"message\":{\"content\":[{\"text\":\"abcdefghijklmnopqrst\"}]}}\n",
    );
    assert_eq!(
        sdk(&paths, "ctx.host.cursorLogs.queryDaily({since:'20261001'})")["data"]["daily"][0]["totalTokens"],
        6
    );
    let changed = events();
    assert_eq!(paths_for(&changed, "parsed"), vec![changed_path.clone()]);
    assert_eq!(paths_for(&changed, "read"), vec![changed_path]);
    assert_eq!(
        paths_for(&changed, "reused"),
        vec![
            paths
                .root
                .join(".cursor/projects/demo/agent-transcripts/b.jsonl")
        ]
    );
    assert_eq!(paths_for(&changed, "aggregated").len(), 1);
    println!(
        "cache evidence: Codex repeat parse=0/reuse=2, change parse=1/reuse=1; Grok repeat parse=0/reuse=1, change parse=1/reuse=0; Cursor repeat parse=0/reuse=2, change parse=1/reuse=1; all recompute daily aggregation"
    );
}

#[test]
fn native_logs_checked_totals_local_dates_and_invalid_cost_fallback() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::analytics::tests::native_logs_checked_totals_local_dates_and_invalid_cost_fallback",
    ) else {
        return;
    };
    // Only this hermetic child changes its own timezone, before any local-date use.
    unsafe {
        std::env::set_var("TZ", "America/Los_Angeles");
    }
    let mut large = claude("large", "2026-10-24T01:00:00Z", 2_000_000_000, Some(0.0));
    large["message"]["usage"]["output_tokens"] = json!(2_000_000_000);
    large["message"]["usage"]["cache_creation_input_tokens"] = json!(2_000_000_000);
    large["message"]["usage"]["cache_read_input_tokens"] = json!(2_000_000_000);
    let early = claude("early", "2026-10-23T06:00:00Z", 100, Some(100.0));
    let fallback = claude("negative-cost", "2026-10-24T10:00:00Z", 100, Some(-1.0));
    put(
        &paths.root,
        ".claude/projects/demo/a.jsonl",
        &format!("{large}\n{early}\n{fallback}\n"),
    );
    let result = sdk(&paths, "ctx.host.claudeLogs.queryDaily({since:'20261023'})");
    let rows = result["data"]["daily"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["date"], "2026-10-24");
    assert_eq!(rows[0]["totalTokens"], 200);
    close(&rows[0]["totalCost"], 0.001134);
    assert_eq!(rows[1]["date"], "2026-10-23");
    assert_eq!(rows[1]["totalTokens"], 8_000_000_000i64);
    close(&rows[1]["totalCost"], 0.0);
    let large_codex = json!({"type":"event_msg","timestamp":"2026-10-24T10:00:00Z","payload":{"type":"token_count","model":"gpt-5.6-sol","info":{"total_token_usage":{"input_tokens":2_000_000_000i64,"cached_input_tokens":2_000_000_000i64,"output_tokens":2_000_000_000i64,"reasoning_output_tokens":2_000_000_000i64}}}});
    put(
        &paths.root,
        ".codex/sessions/large.jsonl",
        &format!("{large_codex}\n"),
    );
    let result = sdk(&paths, "ctx.host.codexLogs.queryDaily({since:'20261023'})");
    assert_eq!(result["data"]["daily"][0]["totalTokens"], 6_000_000_000i64);
    assert_eq!(result["data"]["daily"][0]["outputTokens"], 4_000_000_000i64);
    close(&result["data"]["daily"][0]["totalCost"], 121600.0);
    let mut n = i64::MAX;
    assert!(add_tokens(&mut n, 1).is_err());
    let mut cost = f64::MAX;
    assert!(add_cost(&mut cost, f64::MAX).is_err());

    let transcript = put(
        &paths.root,
        ".cursor/projects/demo/agent-transcripts/a.jsonl",
        "{\"message\":{\"content\":[{\"text\":\"abcdefgh\"}]}}\n",
    );
    let modified = chrono::DateTime::parse_from_rfc3339("2026-10-24T01:00:00Z").unwrap();
    fs::File::options()
        .write(true)
        .open(transcript)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(
            std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(modified.timestamp() as u64),
        ))
        .unwrap();
    let result = sdk(&paths, "ctx.host.cursorLogs.queryDaily({since:'20261023'})");
    assert_eq!(
        result["data"]["daily"][0],
        json!({"date":"2026-10-23","totalTokens":2,"estimated":true})
    );
    assert_eq!(
        sdk(&paths, "ctx.host.cursorLogs.queryDaily({since:'20261024'})")["status"],
        "no_data"
    );
    put(
        &paths.root,
        ".claude/projects/demo/a.jsonl",
        &format!(
            "{}\n{}\n",
            claude("overflow-a", "2026-10-24T10:00:00Z", 1, Some(f64::MAX)),
            claude("overflow-b", "2026-10-24T10:00:00Z", 1, Some(f64::MAX))
        ),
    );
    let overflow = sdk(&paths, "ctx.host.claudeLogs.queryDaily({since:'20261023'})");
    assert_eq!(overflow["status"], "error");
    assert!(
        overflow["error"]
            .as_str()
            .unwrap()
            .contains("cost total overflow")
    );
}

#[cfg(unix)]
#[test]
fn native_logs_policy_checks_discovered_symlinks_and_cache_identity() {
    use std::os::unix::fs::symlink;
    let Some(paths) = hermetic::enter(
        "plugin_engine::analytics::tests::native_logs_policy_checks_discovered_symlinks_and_cache_identity",
    ) else {
        return;
    };
    let a = put(
        &paths.root,
        "first/projects/demo/a.jsonl",
        &format!("{}\n", claude("a", "2026-10-24T10:00:00Z", 100, Some(1.0))),
    );
    let b = put(
        &paths.root,
        "second/projects/demo/b.jsonl",
        &format!("{}\n", claude("b", "2026-10-24T10:00:00Z", 200, Some(2.0))),
    );
    symlink(paths.root.join("first"), paths.root.join("selected")).unwrap();
    symlink(
        paths.root.join("first/projects"),
        paths.root.join("first/projects/demo/cycle"),
    )
    .unwrap();
    let call = "ctx.host.claudeLogs.queryDaily({since:'20261001',homePath:'~/selected'})";
    assert_eq!(sdk(&paths, call)["data"]["daily"][0]["totalTokens"], 200);
    assert_eq!(paths_for(&events(), "parsed"), vec![a]);
    assert_eq!(sdk(&paths, call)["data"]["daily"][0]["totalTokens"], 200);
    assert_eq!(paths_for(&events(), "reused").len(), 1);
    fs::remove_file(paths.root.join("selected")).unwrap();
    symlink(paths.root.join("second"), paths.root.join("selected")).unwrap();
    assert_eq!(sdk(&paths, call)["data"]["daily"][0]["totalTokens"], 300);
    assert_eq!(paths_for(&events(), "parsed"), vec![b]);
    let outside = tempfile::tempdir_in(paths.root.parent().unwrap()).unwrap();
    let sentinel = put(
        outside.path(),
        "sentinel.jsonl",
        &format!(
            "{}\n",
            claude("secret", "2026-10-24T10:00:00Z", 100, Some(777.0))
        ),
    );
    for (directory, name, api) in [
        (".claude/projects/demo", "escape.jsonl", "claudeLogs"),
        (".codex/sessions", "escape.jsonl", "codexLogs"),
        (".grok/sessions/demo", "updates.jsonl", "grokLogs"),
        (
            ".cursor/projects/demo/agent-transcripts",
            "escape.jsonl",
            "cursorLogs",
        ),
    ] {
        fs::create_dir_all(paths.root.join(directory)).unwrap();
        symlink(&sentinel, paths.root.join(directory).join(name)).unwrap();
        let result = sdk(
            &paths,
            &format!("ctx.host.{api}.queryDaily({{since:'20261001'}})"),
        );
        assert_eq!(result["status"], "error", "{api}: {result}");
        assert!(!result.to_string().contains("777"));
    }
    let pi_escape = paths.root.join(".pi/agent/sessions/escape.jsonl");
    fs::create_dir_all(pi_escape.parent().unwrap()).unwrap();
    symlink(&sentinel, &pi_escape).unwrap();
    assert_eq!(
        sdk(&paths, call)["status"],
        "error",
        "Pi fold-in must share the same confinement"
    );
    fs::remove_file(pi_escape).unwrap();
    let policy = FilesystemPolicy::sandbox(&paths.root).unwrap();
    assert_eq!(
        query(
            Source::Claude,
            &json!({"homePath":outside.path(),"since":"20261001"}).to_string(),
            &policy
        )["status"],
        "error"
    );
    assert!(fs::read_to_string(sentinel).unwrap().contains("777"));
    fs::remove_file(paths.root.join(".grok/sessions/demo/updates.jsonl")).unwrap();
    let allowed = put(
        &paths.root,
        "allowed-ledger",
        &format!("{}\n", grok("symlink", "2026-10-24T10:00:00Z", 0.0)),
    );
    symlink(
        allowed,
        paths.root.join(".grok/sessions/demo/updates.jsonl"),
    )
    .unwrap();
    assert_eq!(
        sdk(&paths, "ctx.host.grokLogs.queryDaily({since:'20261001'})")["data"]["daily"][0]["totalTokens"],
        130
    );
}

#[test]
fn native_grok_plugin_builds_spend_and_chart_from_local_sdk_without_auth_or_http() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::analytics::tests::native_grok_plugin_builds_spend_and_chart_from_local_sdk_without_auth_or_http",
    ) else {
        return;
    };
    let now = Utc::now().to_rfc3339();
    put(
        &paths.root,
        ".grok/sessions/demo/updates.jsonl",
        &format!("{}\n", grok("native", &now, 2_000_000_000.0)),
    );
    let plugin = manifest::load_plugins_from_dir(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/crossusage/plugins"),
    )
    .into_iter()
    .find(|p| p.manifest.id == "grok")
    .unwrap();
    let result = runtime::run_probe_in_sandbox(
        &plugin,
        &paths.app_data,
        "test",
        Some(&paths.overrides),
        &paths.root,
    );
    let value = serde_json::to_value(&result.outputs[0]).unwrap();
    let lines = value["lines"].as_array().unwrap();
    assert!(
        lines
            .iter()
            .all(|line| line["type"] != "badge" || line["label"] != "Error"),
        "{value}"
    );
    let chart = lines
        .iter()
        .find(|line| line["type"] == "barChart")
        .unwrap();
    assert_eq!(chart["label"], "Usage Trend");
    assert!(
        chart["points"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["value"] == json!(130.0)),
        "{chart}"
    );
    let today = lines.iter().find(|line| line["label"] == "Today").unwrap();
    assert_eq!(today["value"], "$0.20 · 130 tokens");
    println!(
        "native Grok local-only branch: populated spend + Usage Trend; no auth.json, no HTTP fixture or real credentials"
    );
}

#[test]
fn native_logs_discovery_preserves_source_specific_file_caps() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::analytics::tests::native_logs_discovery_preserves_source_specific_file_caps",
    ) else {
        return;
    };
    for index in 0..501 {
        put(
            &paths.root,
            &format!(".cursor/projects/demo/agent-transcripts/{index}.jsonl"),
            "{\"message\":{\"content\":[{\"text\":\"abcd\"}]}}\n",
        );
    }
    // Cursor's pinned transcript scanner has no newest-500 cap; do not inherit another source's cap.
    let result = sdk(&paths, "ctx.host.cursorLogs.queryDaily({since:'20261001'})");
    assert_eq!(result["data"]["daily"][0]["totalTokens"], 501);
    assert_eq!(paths_for(&events(), "parsed").len(), 501);
    let mut files = (0..501)
        .map(|index| {
            (
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(index),
                1u64,
            )
        })
        .collect::<Vec<_>>();
    super::log_usage_types::cap_log_files_by_mtime(&mut files, |file| file.0, |file| file.1);
    assert_eq!(
        files.len(),
        500,
        "Claude/Codex/Pi/Grok retain the pinned newest-file bound"
    );
    assert!(
        !files
            .iter()
            .any(|file| file.0 == std::time::SystemTime::UNIX_EPOCH)
    );
}

#[cfg(unix)]
#[test]
fn native_logs_review_codex_aliases_do_not_consume_file_budget() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::analytics::tests::native_logs_review_codex_aliases_do_not_consume_file_budget",
    ) else {
        return;
    };
    let start = chrono::DateTime::parse_from_rfc3339("2026-10-24T10:00:00Z").unwrap();
    for index in 0..300 {
        let timestamp = (start + chrono::Duration::seconds(index)).to_rfc3339();
        let path = put(
            &paths.root,
            &format!(".codex/sessions/{index:03}.jsonl"),
            &format!("{}\n", codex(&timestamp)),
        );
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(
                std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs((start.timestamp() + index) as u64),
            ))
            .unwrap();
    }
    let mut archived = codex("2026-10-24T13:00:00Z");
    archived["payload"]["info"]["total_token_usage"]["total_tokens"] = json!(99999);
    put(
        &paths.root,
        ".codex/archived_sessions/000.jsonl",
        &format!("{archived}\n"),
    );
    let home = paths.root.join(".codex");
    let alias = paths.root.join("codex-alias");
    std::os::unix::fs::symlink(&home, &alias).unwrap();
    let mut totals = Vec::new();
    for second in [&home, &alias] {
        let opts =
            json!({"since":"20261001","homePath":format!("{},{}",home.display(),second.display())});
        let result = sdk(&paths, &format!("ctx.host.codexLogs.queryDaily({opts})"));
        assert_eq!(result["status"], "ok");
        totals.push(result["data"]["daily"][0]["totalTokens"].as_i64().unwrap());
    }
    assert_eq!(
        totals,
        vec![42_000, 42_000],
        "all 300 distinct live files must survive repeated/aliased homes; archived relative duplicates stay excluded"
    );
    println!(
        "review Codex repeated + alias homes: 300 distinct files, 42000 tokens each, archived relative duplicate excluded"
    );
}

#[test]
fn native_logs_review_file_valued_directory_roots_are_errors() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::analytics::tests::native_logs_review_file_valued_directory_roots_are_errors",
    ) else {
        return;
    };
    let mut actual = Vec::new();
    for (relative, object) in [
        (".claude/projects", "claudeLogs"),
        (".codex/sessions", "codexLogs"),
        (".grok/sessions", "grokLogs"),
        (".cursor/projects/demo/agent-transcripts", "cursorLogs"),
        (".pi/agent/sessions", "claudeLogs"),
    ] {
        let body = format!("ctx.host.{object}.queryDaily({{since:'20260101'}})");
        assert_eq!(
            sdk(&paths, &body)["status"],
            "no_data",
            "missing {relative}"
        );
        let path = put(&paths.root, relative, "invalid source layout");
        actual.push(sdk(&paths, &body)["status"].as_str().unwrap().to_owned());
        fs::remove_file(path).unwrap();
    }
    assert_eq!(
        actual,
        vec!["error"; 5],
        "existing required directory roots must not masquerade as missing sources"
    );
    println!(
        "review source layouts: missing roots=no_data; file-valued Claude/Codex/Grok/Cursor/Pi roots=error"
    );
}

#[test]
fn native_logs_review_havana_skipped_midnight_uses_calendar_date() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::analytics::tests::native_logs_review_havana_skipped_midnight_uses_calendar_date",
    ) else {
        return;
    };
    unsafe {
        std::env::set_var("TZ", "America/Havana");
    }
    println!(
        "Havana direct date resolution: {:?}; options: {:?}",
        Local.from_local_datetime(
            &NaiveDate::from_ymd_opt(2026, 3, 8)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        ),
        Options::parse("{\"since\":\"20260308\"}").map(|o| o.since)
    );
    let first = "2026-03-08T05:00:00Z";
    let previous = "2026-03-08T04:59:59Z";
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(first)
            .unwrap()
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        "2026-03-08 01:00"
    );
    put(
        &paths.root,
        ".claude/projects/demo/date.jsonl",
        &format!(
            "{}\n{}\n",
            claude("first", first, 100, Some(0.0)),
            claude("previous", previous, 900, Some(0.0))
        ),
    );
    put(
        &paths.root,
        ".codex/sessions/first.jsonl",
        &format!("{}\n", codex(first)),
    );
    put(
        &paths.root,
        ".codex/sessions/previous.jsonl",
        &format!("{}\n", codex(previous)),
    );
    put(
        &paths.root,
        ".pi/agent/sessions/date.jsonl",
        &format!(
            "{}\n{}\n{}\n{}\n",
            pi("claude-first", "anthropic", first, 0.5),
            pi("claude-previous", "anthropic", previous, 0.5),
            pi("codex-first", "openai-codex", first, 0.5),
            pi("codex-previous", "openai-codex", previous, 0.5)
        ),
    );
    put(
        &paths.root,
        ".grok/sessions/date/updates.jsonl",
        &format!(
            "{}\n{}\n",
            grok("first", first, 2_000_000_000.0),
            grok("previous", previous, 2_000_000_000.0)
        ),
    );
    for (name, timestamp) in [("first", first), ("previous", previous)] {
        let path = put(
            &paths.root,
            &format!(".cursor/projects/demo/agent-transcripts/{name}.jsonl"),
            "{\"message\":{\"content\":[{\"text\":\"abcd\"}]}}\n",
        );
        let seconds = chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds as u64),
            ))
            .unwrap();
    }
    for (source, expected) in [
        ("claudeLogs", 218),
        ("codexLogs", 158),
        ("grokLogs", 130),
        ("cursorLogs", 1),
    ] {
        let result = sdk(
            &paths,
            &format!("ctx.host.{source}.queryDaily({{since:'20260308'}})"),
        );
        assert_eq!(result["status"], "ok", "{source}: {result}");
        assert_eq!(result["data"]["daily"].as_array().unwrap().len(), 1);
        assert_eq!(result["data"]["daily"][0]["date"], "2026-03-08");
        assert_eq!(result["data"]["daily"][0]["totalTokens"], expected);
    }
    let db = put(
        &paths.root,
        ".gemini/antigravity/conversations/current.db",
        "",
    );
    let connection = rusqlite::Connection::open(db).unwrap();
    connection
        .execute_batch("CREATE TABLE gen_metadata(idx INTEGER PRIMARY KEY,data BLOB)")
        .unwrap();
    for (idx, timestamp) in [(0, first), (1, previous)] {
        let seconds = chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp() as u64;
        let blob = super::antigravity_proto::encode_generation_blob(
            "gemini-3.6-flash",
            1,
            2,
            3,
            4,
            seconds,
        );
        connection
            .execute(
                "INSERT INTO gen_metadata VALUES(?1,?2)",
                rusqlite::params![idx, blob],
            )
            .unwrap();
    }
    drop(connection);
    let policy = FilesystemPolicy::sandbox(&paths.root).unwrap();
    let rows = super::antigravity_db_usage_scanner::query(
        &scan::ScanContext { policy: &policy },
        &Options::parse("{\"since\":\"20260308\"}").unwrap(),
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].date, "2026-03-08");
    assert_eq!(rows[0].total_tokens, 10);
    println!(
        "review Havana: all five sources + Pi include first existing instant 05:00Z, exclude preceding local day despite missing midnight"
    );
}

#[test]
fn native_logs_review_apia_wholly_skipped_calendar_date() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::analytics::tests::native_logs_review_apia_wholly_skipped_calendar_date",
    ) else {
        return;
    };
    unsafe {
        std::env::set_var("TZ", "Pacific/Apia");
    }
    println!(
        "Apia direct date resolution: {:?}; options: {:?}",
        Local.from_local_datetime(
            &NaiveDate::from_ymd_opt(2011, 12, 30)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        ),
        Options::parse("{\"since\":\"20111230\"}").map(|o| o.since)
    );
    let first = "2011-12-30T10:00:00Z";
    let previous = "2011-12-30T09:59:59Z";
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(first)
            .unwrap()
            .with_timezone(&Local)
            .date_naive()
            .to_string(),
        "2011-12-31"
    );
    put(
        &paths.root,
        ".claude/projects/demo/date.jsonl",
        &format!(
            "{}\n{}\n",
            claude("first", first, 100, Some(0.0)),
            claude("previous", previous, 900, Some(0.0))
        ),
    );
    let result = sdk(&paths, "ctx.host.claudeLogs.queryDaily({since:'20111230'})");
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(result["data"]["daily"].as_array().unwrap().len(), 1);
    assert_eq!(result["data"]["daily"][0]["date"], "2011-12-31");
    assert_eq!(result["data"]["daily"][0]["totalTokens"], 200);
    println!(
        "review Apia skipped 2011-12-30: preceding local date excluded, next existing local date included without invented cutoff instant"
    );
}

#[test]
fn native_logs_review_claude_supplement_fast_rates_are_applied_once() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::analytics::tests::native_logs_review_claude_supplement_fast_rates_are_applied_once",
    ) else {
        return;
    };
    let mut actual = Vec::new();
    for (index, (model, fast, expected)) in [
        ("claude-opus-5", false, 0.03),
        ("claude-opus-5", true, 0.06),
        ("claude-opus-5-fast", false, 0.06),
        ("claude-opus-5-fast", true, 0.06),
    ]
    .into_iter()
    .enumerate()
    {
        let mut row = claude(&format!("case-{index}"), "2026-10-24T10:00:00Z", 1000, None);
        row["message"]["model"] = json!(model);
        row["message"]["usage"] = json!({"input_tokens":1000,"output_tokens":1000,"speed":if fast {"fast"} else {"standard"}});
        let home = paths.root.join(format!("claude-case-{index}"));
        put(&home, "projects/demo/usage.jsonl", &format!("{row}\n"));
        let opts = json!({"since":"20261001","homePath":home});
        let result = sdk(&paths, &format!("ctx.host.claudeLogs.queryDaily({opts})"));
        assert_eq!(result["data"]["daily"][0]["totalTokens"], 2000);
        actual.push((
            result["data"]["daily"][0]["totalCost"].as_f64().unwrap(),
            expected,
        ));
    }
    for (cost, expected) in actual {
        assert!(
            (cost - expected).abs() < 1e-12,
            "actual {cost}, expected {expected}"
        );
    }
    println!(
        "review Claude Opus5: standard=$0.03; speed fast=$0.06; explicit fast model=$0.06 with/without speed flag"
    );
}
