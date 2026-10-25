#[path = "support/hermetic.rs"]
mod hermetic;

use hermetic::HermeticPaths;
use openusage_cli::plugin_engine::{manifest, runtime};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

fn probe(
    paths: &HermeticPaths,
    id: &str,
    replies: &[Value],
    accounts: usize,
) -> (Vec<Value>, Value) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut plugin = manifest::load_plugins_from_dir(&root.join("vendor/crossusage/plugins"))
        .into_iter()
        .find(|plugin| plugin.manifest.id == id)
        .expect("load the actual pinned provider");
    let source = root.join("plugin-overrides").join(format!("{id}.js"));
    fs::copy(source, paths.overrides.join(format!("{id}.js")))
        .expect("install the maintained provider correction");
    let observation = paths.root.join("observed.json");
    plugin.entry_script.push_str(&format!(
        r#"
        ;(function () {{
            var ctx = globalThis.__openusage_ctx;
            var replies = {replies};
            var path = {path};
            var observed = {{requests: [], writes: 0}};
            var write = ctx.host.fs.writeText;
            function persist() {{ write(path, JSON.stringify(observed)); }}
            ctx.host.fs.writeText = function (path, value) {{
                observed.writes += 1;
                persist();
                return write(path, value);
            }};
            ctx.host.http.request = function (request) {{
                observed.requests.push(request);
                persist();
                if (!replies.length) throw new Error("Unexpected fixture request");
                return replies.shift();
            }};
            var previous = globalThis.__openusage_plugin.probe;
            globalThis.__openusage_plugin.probe = function (ctx) {{
                try {{
                    var result = previous(ctx);
                    result.lines.push(ctx.line.progress({{
                        label: "Independent USD", used: 1.25, limit: 10,
                        format: {{kind: "dollars"}}
                    }}));
                    return result;
                }} finally {{
                    observed.centsFormatter = ctx.fmt.dollars(125);
                    persist();
                }}
            }};
            if ({accounts} > 1) {{
                globalThis.__openusage_plugin.discoverAccounts = function () {{
                    var accounts = [];
                    for (var i = 0; i < {accounts}; i++) accounts.push({{id: "fixture-" + i}});
                    return accounts;
                }};
            }}
        }})();
        "#,
        replies = serde_json::to_string(replies).unwrap(),
        path = serde_json::to_string(&observation.to_string_lossy()).unwrap(),
    ));
    let result = runtime::run_probe_in_sandbox(
        &plugin,
        &paths.app_data,
        "test",
        Some(&paths.overrides),
        &paths.root,
    );
    let recorded = fs::read_to_string(observation).unwrap_or_else(|error| {
        panic!(
            "probe did not reach fixture observation: {error}; outputs: {:?}",
            result.outputs
        )
    });
    let observed: Value = serde_json::from_str(&recorded).unwrap();
    assert_eq!(
        observed["writes"], 0,
        "native credentials must not be written"
    );
    assert_eq!(
        observed["centsFormatter"], 1.25,
        "shared formatter still accepts cents"
    );
    let outputs = result
        .outputs
        .iter()
        .map(|output| serde_json::to_value(output).unwrap())
        .collect();
    (outputs, observed)
}

fn reply(status: u16, body: Value) -> Value {
    json!({"status": status, "headers": {}, "bodyText": body.to_string()})
}

fn text(label: &str, value: &str) -> Value {
    json!({"type":"text", "label":label, "value":value, "color":null, "subtitle":null})
}

fn progress(label: &str, used: f64, limit: f64, format: Value) -> Value {
    json!({"type":"progress", "label":label, "used":used, "limit":limit,
        "format":format, "resetsAt":null, "periodDurationMs":null, "color":null})
}

fn lines(output: &Value) -> Vec<Value> {
    let mut lines = output["lines"].as_array().unwrap().clone();
    assert_eq!(
        lines.pop().unwrap(),
        progress("Independent USD", 1.25, 10.0, json!({"kind":"dollars"})),
        "currency corrections must not alter unrelated USD metrics"
    );
    assert!(
        lines.iter().all(|line| line["label"] != "Error"),
        "{output}"
    );
    lines
}

fn set_credentials(id: &str, initial: Option<&str>) {
    // Every caller is already in a fresh, single-case hermetic child.
    unsafe {
        std::env::set_var(format!("{}_API_KEY", id.to_uppercase()), "synthetic-key");
        match initial {
            Some(value) => std::env::set_var("DEEPSEEK_INITIAL_BALANCE", value),
            None => std::env::remove_var("DEEPSEEK_INITIAL_BALANCE"),
        }
    }
}

fn assert_requests(observed: &Value, urls: &[&str], key: &str, timeout: u64) {
    let expected: Vec<Value> = urls
        .iter()
        .map(|url| {
            json!({
                "method":"GET", "url":url,
                "headers":{"Authorization":format!("Bearer {key}"), "Accept":"application/json"},
                "timeoutMs":timeout
            })
        })
        .collect();
    assert_eq!(observed["requests"], json!(expected));
}

#[test]
fn openrouter_override_preserves_usd_units_and_native_requests() {
    let Some(paths) =
        hermetic::enter("openrouter_override_preserves_usd_units_and_native_requests")
    else {
        return;
    };
    set_credentials("openrouter", None);
    let (outputs, observed) = probe(
        &paths,
        "openrouter",
        &[
            reply(200, json!({"data":{"total_credits":10, "total_usage":2.5}})),
            reply(
                200,
                json!({"data":{"usage_daily":1.25, "usage_weekly":2.5,
            "usage_monthly":3.75, "limit":20, "limit_remaining":15, "is_free_tier":false}}),
            ),
        ],
        1,
    );
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0]["plan"], "Pay as you go");
    assert_eq!(
        lines(&outputs[0]),
        vec![
            progress("Credits", 2.5, 10.0, json!({"kind":"dollars"})),
            text("Balance", "$7.5"),
            text("Today", "$1.25"),
            text("This Week", "$2.5"),
            text("This Month", "$3.75"),
            progress("Key Limit", 5.0, 20.0, json!({"kind":"dollars"})),
        ]
    );
    assert_requests(
        &observed,
        &[
            "https://openrouter.ai/api/v1/credits",
            "https://openrouter.ai/api/v1/key",
        ],
        "synthetic-key",
        15000,
    );
}

#[test]
fn openrouter_override_keeps_source_priority_clamping_and_partial_data() {
    let Some(paths) =
        hermetic::enter("openrouter_override_keeps_source_priority_clamping_and_partial_data")
    else {
        return;
    };
    set_credentials("openrouter", None);
    let config = paths.root.join(".config/openusage/openrouter.json");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let bytes = r#"{"apiKey":"synthetic-config-key"}"#;
    fs::write(&config, bytes).unwrap();
    let (outputs, observed) = probe(
        &paths,
        "openrouter",
        &[
            reply(
                200,
                json!({"data":{"total_credits":"10", "total_usage":"12.5"}}),
            ),
            reply(
                200,
                json!({"data":{"usage_daily":"NaN", "usage_weekly":-2.5,
            "usage_monthly":"0", "is_free_tier":true}}),
            ),
        ],
        1,
    );
    assert_eq!(
        lines(&outputs[0]),
        vec![
            progress("Credits", 12.5, 10.0, json!({"kind":"dollars"})),
            text("Balance", "$0"),
            text("This Week", "$0"),
            text("This Month", "$0"),
        ]
    );
    assert_eq!(outputs[0]["plan"], "Free tier");
    assert_requests(
        &observed,
        &[
            "https://openrouter.ai/api/v1/credits",
            "https://openrouter.ai/api/v1/key",
        ],
        "synthetic-config-key",
        15000,
    );
    assert_eq!(fs::read_to_string(config).unwrap(), bytes);
    let (outputs, _) = probe(
        &paths,
        "openrouter",
        &[
            reply(500, json!({"error":"unavailable"})),
            reply(200, json!({"data":{"usage_daily":"4.25"}})),
        ],
        1,
    );
    assert_eq!(lines(&outputs[0]), vec![text("Today", "$4.25")]);
}

#[test]
fn deepseek_override_preserves_cny_without_conversion() {
    let Some(paths) = hermetic::enter("deepseek_override_preserves_cny_without_conversion") else {
        return;
    };
    set_credentials("deepseek", Some("20"));
    let (outputs, observed) = probe(
        &paths,
        "deepseek",
        &[reply(
            200,
            json!({"balance_infos":[{"currency":"CNY", "total_balance":"12.50"}]}),
        )],
        1,
    );
    assert_eq!(outputs.len(), 1);
    assert_eq!(
        lines(&outputs[0]),
        vec![progress(
            "Balance",
            7.5,
            20.0,
            json!({"kind":"currency", "currency":"CNY"})
        )]
    );
    assert_requests(
        &observed,
        &["https://api.deepseek.com/user/balance"],
        "synthetic-key",
        15000,
    );
}

#[test]
fn deepseek_override_preserves_usd_priority_and_per_probe_currency() {
    let Some(paths) =
        hermetic::enter("deepseek_override_preserves_usd_priority_and_per_probe_currency")
    else {
        return;
    };
    set_credentials("deepseek", Some("20"));
    let balances = [
        json!([{"currency":"CNY", "total_balance":10}]),
        json!([{"currency":"CNY", "total_balance":12}, {"currency":"USD", "total_balance":"5.5"}]),
        json!([{"currency":"EUR", "total_balance":8}, {"currency":"CNY", "total_balance":"invalid"}]),
        json!([null, {"currency":"USD", "total_balance":"NaN"}, {"currency":"CNY", "total_balance":"15"}]),
    ];
    let replies: Vec<Value> = balances
        .into_iter()
        .map(|infos| reply(200, json!({"balance_infos":infos})))
        .collect();
    let (outputs, observed) = probe(&paths, "deepseek", &replies, 4);
    assert_eq!(outputs.len(), 4);
    assert_eq!(
        lines(&outputs[0]),
        vec![progress(
            "Balance",
            10.0,
            20.0,
            json!({"kind":"currency", "currency":"CNY"})
        )]
    );
    assert_eq!(
        lines(&outputs[1]),
        vec![progress("Balance", 14.5, 20.0, json!({"kind":"dollars"}))]
    );
    assert_eq!(outputs[2]["lines"][0]["label"], "Error");
    assert!(
        outputs[2]["lines"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Could not find balance")
    );
    assert_eq!(
        lines(&outputs[3]),
        vec![progress(
            "Balance",
            5.0,
            20.0,
            json!({"kind":"currency", "currency":"CNY"})
        )]
    );
    for (i, output) in outputs.iter().enumerate() {
        assert_eq!(output["account"]["id"], format!("fixture-{i}"));
    }
    assert_requests(
        &observed,
        &["https://api.deepseek.com/user/balance"; 4],
        "synthetic-key",
        15000,
    );
}

#[test]
fn provider_metric_overrides_preserve_native_failures() {
    let Some(paths) = hermetic::enter("provider_metric_overrides_preserve_native_failures") else {
        return;
    };
    set_credentials("deepseek", None);
    let (outputs, observed) = probe(&paths, "deepseek", &[], 1);
    assert!(
        outputs[0]["lines"][0]["text"]
            .as_str()
            .unwrap()
            .contains("initial balance missing or invalid")
    );
    assert_eq!(observed["requests"], json!([]));
    set_credentials("deepseek", Some("20"));
    let (outputs, observed) = probe(
        &paths,
        "deepseek",
        &[reply(401, json!({"error":"unauthorized"}))],
        1,
    );
    assert!(
        outputs[0]["lines"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Session expired")
    );
    assert_eq!(observed["requests"].as_array().unwrap().len(), 1);
    set_credentials("openrouter", None);
    let (outputs, observed) = probe(
        &paths,
        "openrouter",
        &[
            reply(401, json!({"error":"unauthorized"})),
            reply(403, json!({"error":"unauthorized"})),
        ],
        1,
    );
    assert!(
        outputs[0]["lines"][0]["text"]
            .as_str()
            .unwrap()
            .contains("API key invalid")
    );
    assert_eq!(observed["requests"].as_array().unwrap().len(), 2);
}

#[test]
fn openrouter_override_rounds_major_units_without_overflow() {
    let Some(paths) = hermetic::enter("openrouter_override_rounds_major_units_without_overflow")
    else {
        return;
    };
    set_credentials("openrouter", None);
    let (outputs, _) = probe(
        &paths,
        "openrouter",
        &[
            reply(200, json!({"data":{}})),
            reply(
                200,
                json!({"data":{"usage_daily":2.675, "usage_weekly":0.004,
            "usage_monthly":1e308}}),
            ),
        ],
        1,
    );
    assert_eq!(
        lines(&outputs[0]),
        vec![
            text("Today", "$2.68"),
            text("This Week", "$0"),
            text("This Month", "$1e+308"),
        ]
    );
}
