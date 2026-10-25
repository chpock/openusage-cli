#[path = "support/hermetic.rs"]
mod hermetic;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hermetic::HermeticPaths;
use openusage_cli::plugin_engine::{manifest, runtime};
use serde_json::{Value, json};
use std::{fs, path::Path};

// Fixtures use the response shapes documented by the pinned providers' plugin.test.js.
// Only HTTP is replaced; credentials, environment, builders and validation remain native.
fn probe(paths: &HermeticPaths, id: &str, exchanges: Vec<Value>) -> (Value, Value) {
    probe_with_readers(paths, id, exchanges, None)
}

// Controlled readers exercise native callers, not the disabled backend implementations.
fn probe_with_readers(
    paths: &HermeticPaths,
    id: &str,
    exchanges: Vec<Value>,
    readers: Option<Vec<Value>>,
) -> (Value, Value) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut plugin = manifest::load_plugins_from_dir(&root.join("vendor/crossusage/plugins"))
        .into_iter()
        .find(|plugin| plugin.manifest.id == id)
        .expect("load pinned native provider");
    let observation = paths.root.join("observed.json");
    let harness = format!(
        r#";(function () {{
            var ctx = globalThis.__openusage_ctx;
            var exchanges = {exchanges}, path = {path};
            var readers = {readers};
            var observed = {{requests: [], writes: [], violations: [], readers: []}};
            var write = ctx.host.fs.writeText;
            function persist() {{ write(path, JSON.stringify(observed)); }}
            function canonical(value) {{
                if (!value || typeof value !== 'object') return JSON.stringify(value);
                if (Array.isArray(value)) return '[' + value.map(canonical).join(',') + ']';
                return '{{' + Object.keys(value).sort().map(function (key) {{
                    return JSON.stringify(key) + ':' + canonical(value[key]);
                }}).join(',') + '}}';
            }}
            if (readers !== null) {{
                ['sqlite', 'keychain', 'ls'].forEach(function (namespace) {{
                    var object = ctx.host[namespace] || (ctx.host[namespace] = {{}});
                    var names = Object.keys(object);
                    var required = namespace === 'ls' ? ['discover'] : namespace === 'sqlite' ? ['query', 'exec'] :
                        ['readGenericPassword', 'readGenericPasswordForCurrentUser', 'readGenericPasswordForAccount',
                         'writeGenericPassword', 'writeGenericPasswordForCurrentUser', 'writeGenericPasswordForAccount'];
                    required.forEach(function (name) {{ if (names.indexOf(name) < 0) names.push(name); }});
                    names.forEach(function (name) {{
                        ctx.host[namespace][name] = function () {{
                            var call = {{method: namespace + '.' + name, args: Array.prototype.slice.call(arguments)}};
                            var expected = readers[observed.readers.length];
                            observed.readers.push(call); persist();
                            if (!expected || canonical(call) !== canonical(expected.call)) {{
                                observed.violations.push(call); persist();
                                throw new Error('Unexpected sensitive-reader fixture call');
                            }}
                            if (expected.error) throw new Error(expected.error);
                            return expected.value;
                        }};
                    }});
                }});
            }}
            ctx.host.fs.writeText = function (path, text) {{
                observed.writes.push({{path: path, text: text}}); persist();
                return write(path, text);
            }};
            ctx.host.http.request = function (request) {{
                var index = observed.requests.length;
                observed.requests.push(request); persist();
                var exchange = exchanges[index];
                if (!exchange || canonical(request) !== canonical(exchange.request)) {{
                    observed.violations.push(request); persist();
                    throw new Error('Unexpected fixture request');
                }}
                if (exchange.networkError) throw new Error('Synthetic transport failure');
                return exchange.reply;
            }};
            persist();
        }})();
        "#,
        exchanges = json!(exchanges),
        readers = json!(readers),
        path = json!(observation.to_string_lossy()),
    );
    plugin.entry_script = harness + plugin.entry_script.as_str();
    let result = runtime::run_probe_in_sandbox(&plugin, &paths.app_data, "test", None, &paths.root);
    let observed: Value =
        serde_json::from_str(&fs::read_to_string(observation).unwrap_or_else(|error| {
            panic!(
                "{id}: recorder initialization failed: {error}; outputs: {:?}",
                result.outputs
            )
        }))
        .unwrap();
    assert_eq!(observed["violations"], json!([]), "{id}: fixture mismatch");
    assert_eq!(
        observed["requests"],
        json!(exchanges.iter().map(|e| &e["request"]).collect::<Vec<_>>()),
        "{id}: all and only expected exchanges must run"
    );
    if let Some(readers) = readers {
        assert_eq!(
            observed["readers"],
            json!(readers.iter().map(|r| &r["call"]).collect::<Vec<_>>()),
            "{id}: all and only expected sensitive reader calls must run"
        );
    }
    assert_eq!(result.outputs.len(), 1, "{id}: {:?}", result.outputs);
    let output = serde_json::to_value(&result.outputs[0]).unwrap();
    assert_eq!(output["providerId"], id);
    assert_eq!(
        output["account"],
        json!({"id":"default","origin":"native","isActive":true})
    );
    (output, observed)
}

fn request(url: &str, key: &str, timeout: u64) -> Value {
    json!({"method":"GET", "url":url, "headers":{
        "Authorization":format!("Bearer {key}"), "Accept":"application/json"}, "timeoutMs":timeout})
}

fn exchange(request: Value, status: u16, body: Value) -> Value {
    json!({"request":request, "reply":{"status":status, "headers":{}, "bodyText":body.to_string()}})
}

fn write(paths: &HermeticPaths, relative: &str, text: &str) {
    let path = paths.root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn env(name: &str, value: &str) {
    // Called only in the single-case hermetic child, never in the parent process.
    unsafe { std::env::set_var(name, value) };
}

fn labels(output: &Value, expected: &[&str]) {
    let lines = output["lines"].as_array().unwrap();
    assert_eq!(
        lines
            .iter()
            .map(|line| line["label"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected,
        "{output}"
    );
    assert!(
        lines.iter().all(|line| line["label"] != "Error"),
        "{output}"
    );
}

fn metric(output: &Value, label: &str, used: f64, limit: f64, format: Value) {
    let line = line(output, label);
    assert_eq!(line["type"], "progress");
    assert_eq!(line["used"], used, "{output}");
    assert_eq!(line["limit"], limit, "{output}");
    assert_eq!(line["format"], format);
}

fn line<'a>(output: &'a Value, label: &str) -> &'a Value {
    output["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|line| line["label"] == label)
        .unwrap_or_else(|| panic!("missing {label}: {output}"))
}

fn error(output: &Value, expected: &str) {
    assert_eq!(output["lines"].as_array().unwrap().len(), 1, "{output}");
    assert_eq!(line(output, "Error")["type"], "badge");
    assert_eq!(line(output, "Error")["text"], expected, "{output}");
}

fn no_writes(observed: &Value) {
    assert_eq!(observed["writes"], json!([]));
}

#[test]
fn amp_file_balance_and_rejected_or_malformed_sources() {
    let Some(paths) = hermetic::enter("amp_file_balance_and_rejected_or_malformed_sources") else {
        return;
    };
    let auth = ".local/share/amp/secrets.json";
    write(
        &paths,
        auth,
        r#"{"apiKey@https://ampcode.com/":"amp-file"}"#,
    );
    let req = json!({"method":"POST", "url":"https://ampcode.com/api/internal", "headers":{
        "Authorization":"Bearer amp-file", "Content-Type":"application/json"},
        "bodyText":r#"{"method":"userDisplayBalanceInfo","params":{}}"#, "timeoutMs":15000});
    let (out, observed) = probe(
        &paths,
        "amp",
        vec![exchange(
            req.clone(),
            200,
            json!({"ok":true,
        "result":{"displayText":"Amp Free: $1,250/$2,000 remaining (replenishes +$2/hour) [+10% bonus for 3 more days]\nIndividual credits: $12.50 remaining"}}),
        )],
    );
    labels(&out, &["Free", "Bonus", "Credits"]);
    metric(&out, "Free", 750.0, 2000.0, json!({"kind":"dollars"}));
    assert_eq!(line(&out, "Free")["periodDurationMs"], 86_400_000);
    assert_eq!(line(&out, "Bonus")["value"], "+10% for 3d");
    assert_eq!(line(&out, "Credits")["value"], "$12.50");
    no_writes(&observed);
    for status in [401, 403] {
        let (out, _) = probe(
            &paths,
            "amp",
            vec![exchange(req.clone(), status, json!({}))],
        );
        error(&out, "Session expired. Re-authenticate in Amp Code.");
    }
    write(&paths, auth, "{bad");
    let (out, observed) = probe(&paths, "amp", vec![]);
    error(&out, "Amp not installed. Install Amp Code to get started.");
    no_writes(&observed);
}

fn command_request(key: &str, endpoint: &str) -> Value {
    let mut req = request(
        &format!("https://api.commandcode.ai/alpha/{endpoint}"),
        key,
        10000,
    );
    req["headers"]["Content-Type"] = json!("application/json");
    req["headers"]["User-Agent"] = json!("OpenUsage");
    req
}

#[test]
fn command_code_env_file_precedence_and_partial_failure() {
    let Some(paths) = hermetic::enter("command_code_env_file_precedence_and_partial_failure")
    else {
        return;
    };
    write(
        &paths,
        ".commandcode/auth.json",
        r#"{"apiKey":"file-command"}"#,
    );
    env("COMMAND_CODE_API_KEY", " env-command ");
    for (key, credit_status) in [("env-command", 200), ("file-command", 503)] {
        if key == "file-command" {
            env("COMMAND_CODE_API_KEY", " ");
        }
        let (out, observed) = probe(
            &paths,
            "command-code",
            vec![
                exchange(
                    command_request(key, "whoami"),
                    200,
                    json!({"success":true,"user":{"id":"fixture"}}),
                ),
                exchange(
                    command_request(key, "billing/credits"),
                    credit_status,
                    json!({"credits":{"monthlyCredits":6}}),
                ),
                exchange(
                    command_request(key, "usage/summary"),
                    200,
                    json!({"totalCost":4.125,"totalMonthlyCredits":4,
                "totalTokens":"1250000", "models":[{"model":"vendor/model-a","totalCost":3,"count":12}]}),
                ),
                exchange(
                    command_request(key, "billing/subscriptions"),
                    200,
                    json!({"success":true,"data":{"planId":"individual-go"}}),
                ),
            ],
        );
        if credit_status == 200 {
            labels(
                &out,
                &["Monthly credits", "Total spent", "Tokens used", "Models"],
            );
            metric(
                &out,
                "Monthly credits",
                40.0,
                100.0,
                json!({"kind":"percent"}),
            );
            assert_eq!(
                line(&out, "Monthly credits")["periodDurationMs"],
                2_592_000_000_u64
            );
        } else {
            labels(&out, &["Total spent", "Tokens used", "Models"]);
        }
        assert_eq!(out["plan"], "Individual Go");
        assert_eq!(line(&out, "Total spent")["value"], "$4.13");
        assert_eq!(line(&out, "Tokens used")["value"], "1.3M");
        assert_eq!(line(&out, "Models")["value"], "model-a $3.00 (12 calls)");
        no_writes(&observed);
    }
    let (out, _) = probe(
        &paths,
        "command-code",
        vec![exchange(
            command_request("file-command", "whoami"),
            401,
            json!({}),
        )],
    );
    error(&out, "Command Code API unreachable. Check your connection.");
}

#[test]
fn crofai_real_env_requests_credits_and_validation_failures() {
    let Some(paths) = hermetic::enter("crofai_real_env_requests_credits_and_validation_failures")
    else {
        return;
    };
    env("CROFAI_API_KEY", " crof-key ");
    let req = request("https://crof.ai/usage_api/", "crof-key", 10000);
    let (out, observed) = probe(
        &paths,
        "crofai",
        vec![exchange(
            req.clone(),
            200,
            json!({"credits":12.345,"usable_requests":375,"requests_plan":500}),
        )],
    );
    labels(&out, &["Requests", "Credits"]);
    metric(
        &out,
        "Requests",
        125.0,
        500.0,
        json!({"kind":"count","suffix":"requests"}),
    );
    assert_eq!(line(&out, "Credits")["value"], "$12.35");
    no_writes(&observed);
    for body in [
        json!({"credits":"12"}),
        json!({"usable_requests":10,"requests_plan":0}),
        json!([]),
    ] {
        let (out, _) = probe(&paths, "crofai", vec![exchange(req.clone(), 200, body)]);
        error(&out, "Usage response invalid. Try again later.");
    }
    for (status, message) in [
        (401, "API key invalid. Check your CrofAI API key."),
        (503, "Usage request failed (HTTP 503). Try again later."),
    ] {
        let (out, _) = probe(
            &paths,
            "crofai",
            vec![exchange(req.clone(), status, json!({}))],
        );
        error(&out, message);
    }
    let (out, _) = probe(
        &paths,
        "crofai",
        vec![json!({"request":req,"networkError":true})],
    );
    error(&out, "Usage request failed. Check your connection.");
}

#[test]
fn neuralwatt_real_env_kwh_dollars_period_and_failures() {
    let Some(paths) = hermetic::enter("neuralwatt_real_env_kwh_dollars_period_and_failures") else {
        return;
    };
    env("NEURALWATT_API_KEY", " watt-key ");
    let mut req = request("https://api.neuralwatt.com/v1/quota", "watt-key", 10000);
    req["headers"]["User-Agent"] = json!("OpenUsage");
    let (out, observed) = probe(
        &paths,
        "neuralwatt",
        vec![exchange(
            req.clone(),
            200,
            json!({
        "subscription":{"plan":"standard","current_period_start":"2026-04-11T05:05:25Z",
            "current_period_end":"2026-05-11T05:05:25Z","kwh_included":"20","kwh_used":"13.9023"},
        "balance":{"total_credits_usd":"52.34","credits_used_usd":"19.6626","accounting_method":"energy"}}),
        )],
    );
    labels(&out, &["Subscription", "Balance", "Method"]);
    assert_eq!(out["plan"], "Standard");
    metric(
        &out,
        "Subscription",
        13.9023,
        20.0,
        json!({"kind":"count","suffix":"kWh"}),
    );
    assert_eq!(
        line(&out, "Subscription")["resetsAt"],
        "2026-05-11T05:05:25.000Z"
    );
    assert_eq!(
        line(&out, "Subscription")["periodDurationMs"],
        2_592_000_000_u64
    );
    metric(&out, "Balance", 19.66, 52.34, json!({"kind":"dollars"}));
    assert_eq!(line(&out, "Method")["text"], "Energy");
    no_writes(&observed);
    for (status, body, message) in [
        (401, json!({}), "Invalid API key. Check NEURALWATT_API_KEY."),
        (
            500,
            json!({}),
            "Request failed (HTTP 500). Try again later.",
        ),
        (200, Value::Null, "Response invalid. Try again later."),
    ] {
        let (out, _) = probe(
            &paths,
            "neuralwatt",
            vec![exchange(req.clone(), status, body)],
        );
        error(&out, message);
    }
}

fn minimax_request(cn: bool, key: &str) -> Value {
    let mut req = request(
        if cn {
            "https://api.minimaxi.com/v1/token_plan/remains"
        } else {
            "https://www.minimax.io/v1/token_plan/remains"
        },
        key,
        15000,
    );
    req["headers"]["Content-Type"] = json!("application/json");
    req
}

#[test]
fn minimax_global_cn_counts_percent_and_region_fallback() {
    let Some(paths) = hermetic::enter("minimax_global_cn_counts_percent_and_region_fallback")
    else {
        return;
    };
    env("MINIMAX_API_KEY", "global-key");
    let counts = |total, remaining| {
        json!({"base_resp":{"status_code":0},"model_remains":[{
        "current_interval_total_count":total,"current_interval_usage_count":remaining,
        "start_time":1700000000000_u64,"end_time":1700018000000_u64}]})
    };
    let (out, _) = probe(
        &paths,
        "minimax",
        vec![exchange(
            minimax_request(false, "global-key"),
            200,
            counts(300, 180),
        )],
    );
    labels(&out, &["Session"]);
    metric(
        &out,
        "Session",
        120.0,
        300.0,
        json!({"kind":"count","suffix":"prompts"}),
    );
    assert_eq!(out["plan"], "Plus (GLOBAL)");
    assert_eq!(line(&out, "Session")["periodDurationMs"], 18_000_000);
    assert_eq!(
        line(&out, "Session")["resetsAt"],
        "2023-11-15T03:13:20.000Z"
    );
    env("MINIMAX_CN_API_KEY", "cn-key");
    let (out, _) = probe(
        &paths,
        "minimax",
        vec![exchange(
            minimax_request(true, "cn-key"),
            200,
            counts(1500, 900),
        )],
    );
    labels(&out, &["Session"]);
    metric(
        &out,
        "Session",
        40.0,
        100.0,
        json!({"kind":"count","suffix":"prompts"}),
    );
    assert_eq!(out["plan"], "Plus (CN)");
    let percent = json!({"data":{"plan_name":"Token Plan","model_remains":[
        {"model_name":"other","current_interval_remaining_percent":90},
        {"model_name":"general","current_interval_remaining_percent":"65"}]}});
    let (out, _) = probe(
        &paths,
        "minimax",
        vec![exchange(
            minimax_request(true, "cn-key"),
            200,
            percent.clone(),
        )],
    );
    labels(&out, &["Session"]);
    metric(&out, "Session", 35.0, 100.0, json!({"kind":"percent"}));
    let (out, _) = probe(
        &paths,
        "minimax",
        vec![
            exchange(minimax_request(true, "cn-key"), 401, json!({})),
            exchange(minimax_request(true, "cn-key"), 401, json!({})),
            exchange(minimax_request(false, "global-key"), 200, percent),
        ],
    );
    labels(&out, &["Session"]);
    assert_eq!(out["plan"], "Token Plan (GLOBAL)");
    metric(&out, "Session", 35.0, 100.0, json!({"kind":"percent"}));
    let (out, _) = probe(
        &paths,
        "minimax",
        vec![
            exchange(
                minimax_request(true, "cn-key"),
                200,
                json!({"model_remains":[]}),
            ),
            exchange(
                minimax_request(false, "global-key"),
                200,
                json!({"model_remains":[]}),
            ),
        ],
    );
    error(&out, "Could not parse usage data.");
}

#[test]
fn synthetic_source_precedence_numeric_envelopes_and_badges() {
    let Some(paths) = hermetic::enter("synthetic_source_precedence_numeric_envelopes_and_badges")
    else {
        return;
    };
    env("SYNTHETIC_API_KEY", "env-syn");
    write(
        &paths,
        ".local/share/opencode/auth.json",
        r#"{"synthetic":{"key":"oc-syn"}}"#,
    );
    write(
        &paths,
        ".pi/agent/auth.json",
        r#"{"synthetic":{"type":"api_key","key":"pi-syn"}}"#,
    );
    write(
        &paths,
        ".crossusage/config.json",
        r#"{"synthetic":{"apiKey":"cu-syn"}}"#,
    );
    for (key, remove) in [
        ("cu-syn", None),
        ("pi-syn", Some(".crossusage/config.json")),
        ("oc-syn", Some(".pi/agent/auth.json")),
        ("env-syn", Some(".local/share/opencode/auth.json")),
    ] {
        if let Some(path) = remove {
            fs::remove_file(paths.root.join(path)).unwrap();
        }
        let (out, observed) = probe(
            &paths,
            "synthetic",
            vec![exchange(
                request("https://api.synthetic.new/v2/quotas", key, 15000),
                200,
                json!({"data":{
                "rolling_five_hour_limit":{"max":"600","remaining":"450"},
                "weekly_token_limit":{"percentRemaining":"75"},
                "subscription":{"limit":999,"requests":998}}}),
            )],
        );
        labels(&out, &["5h Rate Limit", "Mana Bar"]);
        metric(
            &out,
            "5h Rate Limit",
            150.0,
            600.0,
            json!({"kind":"count","suffix":"requests"}),
        );
        metric(&out, "Mana Bar", 25.0, 100.0, json!({"kind":"percent"}));
        no_writes(&observed);
    }
    assert!(!paths.root.join(".crossusage/config.json").exists());
    let req = request("https://api.synthetic.new/v2/quotas", "env-syn", 15000);
    let (out, _) = probe(
        &paths,
        "synthetic",
        vec![exchange(req.clone(), 200, json!({"unknown":true}))],
    );
    labels(&out, &["Quotas"]);
    assert_eq!(line(&out, "Quotas")["type"], "badge");
    let (out, _) = probe(
        &paths,
        "synthetic",
        vec![exchange(req.clone(), 401, json!({}))],
    );
    error(
        &out,
        "API key invalid or expired. Check your Synthetic API key.",
    );
    let (out, _) = probe(&paths, "synthetic", vec![exchange(req, 200, Value::Null)]);
    error(&out, "Could not parse usage data.");
}

#[test]
fn zai_token_credit_precedence_numeric_strings_resets_and_invalid_envelopes() {
    let Some(paths) =
        hermetic::enter("zai_token_credit_precedence_numeric_strings_resets_and_invalid_envelopes")
    else {
        return;
    };
    env("ZAI_API_KEY", "zai-key");
    env("GLM_API_KEY", "unused-glm-key");
    let sub = request(
        "https://api.z.ai/api/biz/subscription/list",
        "zai-key",
        10000,
    );
    let quota = request(
        "https://api.z.ai/api/monitor/usage/quota/limit",
        "zai-key",
        10000,
    );
    let (out, observed) = probe(
        &paths,
        "zai",
        vec![
            exchange(
                sub.clone(),
                200,
                json!({"data":[{"productName":"GLM Coding Max"}]}),
            ),
            exchange(
                quota.clone(),
                200,
                json!({"data":{"limits":[
            {"type":"CREDIT_LIMIT","unit":3,"percentage":99},
            {"type":"TOKENS_LIMIT","unit":3,"percentage":"12.5","nextResetTime":"1738368000000"},
            {"type":"CREDIT_LIMIT","unit":6,"percentage":"40"},
            {"type":"TIME_LIMIT","usage":"4000","currentValue":"1095","nextResetTime":1738368000000_u64}]}}),
            ),
        ],
    );
    labels(&out, &["Session", "Weekly", "Web Searches"]);
    metric(&out, "Session", 12.5, 100.0, json!({"kind":"percent"}));
    metric(&out, "Weekly", 40.0, 100.0, json!({"kind":"percent"}));
    metric(
        &out,
        "Web Searches",
        1095.0,
        4000.0,
        json!({"kind":"count","suffix":"/ 4000"}),
    );
    assert_eq!(
        line(&out, "Session")["resetsAt"],
        "2025-02-01T00:00:00.000Z"
    );
    assert_eq!(line(&out, "Session")["periodDurationMs"], 18_000_000);
    assert_eq!(line(&out, "Weekly")["periodDurationMs"], 604_800_000);
    no_writes(&observed);
    for body in [
        json!({"data":[]}),
        json!({"limits":[{"type":"TOKENS_LIMIT","unit":3,"percentage":true}]}),
    ] {
        let (out, _) = probe(
            &paths,
            "zai",
            vec![
                exchange(sub.clone(), 503, json!({})),
                exchange(quota.clone(), 200, body),
            ],
        );
        error(&out, "Usage response invalid. Try again later.");
    }
    for body in [
        json!({"limits":[]}),
        json!({"limits":[{"type":"UNKNOWN","percentage":25}]}),
    ] {
        let (out, _) = probe(
            &paths,
            "zai",
            vec![
                exchange(sub.clone(), 503, json!({})),
                exchange(quota.clone(), 200, body),
            ],
        );
        labels(&out, &["Status"]);
        assert_eq!(line(&out, "Status")["text"], "No usage data");
    }
    let (out, _) = probe(
        &paths,
        "zai",
        vec![
            exchange(sub, 503, json!({})),
            exchange(quota, 403, json!({})),
        ],
    );
    error(&out, "API key invalid. Check your Z.ai API key.");
}

fn kimi_usage() -> Value {
    json!({"user":{"membership":{"level":"LEVEL_INTERMEDIATE"}},
        "usage":{"limit":"100","remaining":"74","resetTime":"2099-02-11T00:00:00Z"},
        "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
            "detail":{"limit":"200","remaining":"170","resetTime":"2099-02-07T00:00:00Z"}}]})
}

fn kimi_request(token: &str) -> Value {
    let mut req = request("https://api.kimi.com/coding/v1/usages", token, 10000);
    req["headers"]["User-Agent"] = json!("OpenUsage");
    req
}

fn kimi_refresh() -> Value {
    json!({"method":"POST","url":"https://auth.kimi.com/api/oauth/token","headers":{
        "Content-Type":"application/x-www-form-urlencoded","Accept":"application/json"},
        "bodyText":"client_id=17e5f671-d194-4dfb-9706-5516cb48c098&grant_type=refresh_token&refresh_token=refresh%2Bfixture",
        "timeoutMs":15000})
}

#[test]
fn kimi_file_auth_refresh_writeback_and_rejected_refresh() {
    let Some(paths) = hermetic::enter("kimi_file_auth_refresh_writeback_and_rejected_refresh")
    else {
        return;
    };
    let auth = ".kimi/credentials/kimi-code.json";
    write(
        &paths,
        auth,
        r#"{"access_token":"old-kimi","refresh_token":"refresh+fixture","expires_at":1}"#,
    );
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let (out, observed) = probe(
        &paths,
        "kimi",
        vec![
            exchange(
                kimi_refresh(),
                200,
                json!({"access_token":"new-kimi","refresh_token":"rotated-kimi",
            "expires_in":3600,"scope":"kimi-code","token_type":"Bearer"}),
            ),
            exchange(kimi_request("new-kimi"), 200, kimi_usage()),
        ],
    );
    labels(&out, &["Session", "Weekly"]);
    metric(&out, "Session", 15.0, 100.0, json!({"kind":"percent"}));
    metric(&out, "Weekly", 26.0, 100.0, json!({"kind":"percent"}));
    assert_eq!(out["plan"], "Intermediate");
    assert_eq!(line(&out, "Session")["periodDurationMs"], 18_000_000);
    let stored: Value =
        serde_json::from_str(&fs::read_to_string(paths.root.join(auth)).unwrap()).unwrap();
    assert_eq!(stored["access_token"], "new-kimi");
    assert_eq!(stored["refresh_token"], "rotated-kimi");
    assert_eq!(stored["scope"], "kimi-code");
    assert_eq!(stored["token_type"], "Bearer");
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    assert!((before + 3600.0..=after + 3600.0).contains(&stored["expires_at"].as_f64().unwrap()));
    assert_eq!(observed["writes"].as_array().unwrap().len(), 1);
    let (out, observed) = probe(
        &paths,
        "kimi",
        vec![exchange(kimi_request("new-kimi"), 200, kimi_usage())],
    );
    labels(&out, &["Session", "Weekly"]);
    no_writes(&observed);
    write(
        &paths,
        auth,
        r#"{"access_token":"old-kimi","refresh_token":"refresh+fixture","expires_at":1}"#,
    );
    let bytes = fs::read(paths.root.join(auth)).unwrap();
    let (out, observed) = probe(
        &paths,
        "kimi",
        vec![exchange(kimi_refresh(), 401, json!({}))],
    );
    error(&out, "Session expired. Run `kimi login` to authenticate.");
    assert_eq!(fs::read(paths.root.join(auth)).unwrap(), bytes);
    no_writes(&observed);
}

fn jwt(exp: u64) -> String {
    format!(
        "e30.{}.fixture",
        URL_SAFE_NO_PAD.encode(json!({"exp":exp}).to_string())
    )
}

fn factory_request(token: &str, get: bool) -> Value {
    let mut req = request(
        "https://api.factory.ai/api/organization/subscription/usage",
        token,
        10000,
    );
    req["headers"]["User-Agent"] = json!("OpenUsage");
    if !get {
        req["method"] = json!("POST");
        req["headers"]["Content-Type"] = json!("application/json");
        req["bodyText"] = json!(r#"{"useCache":true}"#);
    }
    req
}

#[test]
fn factory_file_priority_405_get_and_refresh_writeback() {
    let Some(paths) = hermetic::enter("factory_file_priority_405_get_and_refresh_writeback") else {
        return;
    };
    let token = jwt(4_102_444_800);
    let auth = ".factory/auth.encrypted";
    write(
        &paths,
        auth,
        &json!({"access_token":token,"refresh_token":"factory+refresh"}).to_string(),
    );
    write(
        &paths,
        ".factory/auth.json",
        r#"{"access_token":"unselected"}"#,
    );
    let usage = json!({"usage":{"startDate":1735689600000_u64,"endDate":1738368000000_u64,
        "standard":{"orgTotalTokensUsed":1234567,"totalAllowance":20000000},
        "premium":{"orgTotalTokensUsed":42,"totalAllowance":1000}}});
    let (out, observed) = probe(
        &paths,
        "factory",
        vec![
            exchange(factory_request(&token, false), 405, json!({})),
            exchange(factory_request(&token, true), 200, usage.clone()),
        ],
    );
    labels(&out, &["Standard", "Premium"]);
    metric(
        &out,
        "Standard",
        1_234_567.0,
        20_000_000.0,
        json!({"kind":"count","suffix":"tokens"}),
    );
    metric(
        &out,
        "Premium",
        42.0,
        1000.0,
        json!({"kind":"count","suffix":"tokens"}),
    );
    assert_eq!(out["plan"], "Pro");
    assert_eq!(
        line(&out, "Standard")["periodDurationMs"],
        2_678_400_000_u64
    );
    no_writes(&observed);
    let expired = jwt(1);
    write(
        &paths,
        auth,
        &json!({"access_token":expired,"refresh_token":"factory+refresh"}).to_string(),
    );
    let refresh = json!({"method":"POST","url":"https://api.workos.com/user_management/authenticate",
        "headers":{"Content-Type":"application/x-www-form-urlencoded"},"timeoutMs":15000,
        "bodyText":"grant_type=refresh_token&refresh_token=factory%2Brefresh&client_id=client_01HNM792M5G5G1A2THWPXKFMXB"});
    let (out, observed) = probe(
        &paths,
        "factory",
        vec![
            exchange(
                refresh.clone(),
                200,
                json!({"access_token":token,"refresh_token":"rotated-factory"}),
            ),
            exchange(factory_request(&token, false), 200, usage),
        ],
    );
    labels(&out, &["Standard", "Premium"]);
    let stored: Value =
        serde_json::from_str(&fs::read_to_string(paths.root.join(auth)).unwrap()).unwrap();
    assert_eq!(
        stored,
        json!({"access_token":token,"refresh_token":"rotated-factory"})
    );
    assert_eq!(observed["writes"].as_array().unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(paths.root.join(".factory/auth.json")).unwrap(),
        r#"{"access_token":"unselected"}"#
    );
    write(
        &paths,
        auth,
        &json!({"access_token":expired,"refresh_token":"factory+refresh"}).to_string(),
    );
    let (out, observed) = probe(&paths, "factory", vec![exchange(refresh, 401, json!({}))]);
    error(&out, "Session expired. Run `droid` to log in again.");
    no_writes(&observed);
}

fn devin_request(server: &str) -> Value {
    json!({"method":"POST", "url":format!("{server}/exa.seat_management_pb.SeatManagementService/GetUserStatus"),
        "headers":{"Content-Type":"application/json","Connect-Protocol-Version":"1"},
        "bodyText":r#"{"metadata":{"apiKey":"devin-cli","ideName":"devin","ideVersion":"1.108.2","extensionName":"devin","extensionVersion":"1.108.2","locale":"en"}}"#,
        "timeoutMs":15000})
}

#[test]
fn devin_toml_https_selection_quota_percent_and_micros() {
    let Some(paths) = hermetic::enter("devin_toml_https_selection_quota_percent_and_micros") else {
        return;
    };
    let auth = ".local/share/devin/credentials.toml";
    write(
        &paths,
        auth,
        "windsurf_api_key = \"devin-cli\"\napi_server_url = 'https://fixture.invalid/' # fixture\n",
    );
    let body = json!({"userStatus":{"planStatus":{"planInfo":{"planName":"Max"},
        "dailyQuotaRemainingPercent":"75","weeklyQuotaRemainingPercent":40,
        "overageBalanceMicros":"964220000","dailyQuotaResetAtUnix":"1774080000",
        "weeklyQuotaResetAtUnix":"1774166400"}}});
    let (out, observed) = probe(
        &paths,
        "devin",
        vec![exchange(
            devin_request("https://fixture.invalid"),
            200,
            body.clone(),
        )],
    );
    labels(
        &out,
        &["Daily quota", "Weekly quota", "Extra usage balance"],
    );
    assert_eq!(out["plan"], "Max");
    metric(&out, "Daily quota", 25.0, 100.0, json!({"kind":"percent"}));
    metric(&out, "Weekly quota", 60.0, 100.0, json!({"kind":"percent"}));
    assert_eq!(line(&out, "Extra usage balance")["value"], "$964.22");
    assert_eq!(
        line(&out, "Daily quota")["resetsAt"],
        "2026-03-21T08:00:00.000Z"
    );
    assert_eq!(line(&out, "Weekly quota")["periodDurationMs"], 604_800_000);
    no_writes(&observed);
    write(
        &paths,
        auth,
        "windsurf_api_key = \"devin-cli\"\napi_server_url = \"http://rejected.invalid\"\n",
    );
    let (out, _) = probe(
        &paths,
        "devin",
        vec![exchange(
            devin_request("https://server.codeium.com"),
            200,
            body,
        )],
    );
    labels(
        &out,
        &["Daily quota", "Weekly quota", "Extra usage balance"],
    );
    for (status, message) in [
        (
            401,
            "Run devin auth login or sign in to Devin and try again.",
        ),
        (200, "Devin quota data unavailable. Try again later."),
    ] {
        let (out, _) = probe(
            &paths,
            "devin",
            vec![exchange(
                devin_request("https://server.codeium.com"),
                status,
                json!({"userStatus":{}}),
            )],
        );
        error(&out, message);
    }
    write(&paths, auth, "not TOML credentials");
    let (out, _) = probe(&paths, "devin", vec![]);
    error(
        &out,
        "Run devin auth login or sign in to Devin and try again.",
    );
}

#[test]
fn jetbrains_linux_xml_credit_scaling_and_selected_ide_window() {
    let Some(paths) = hermetic::enter("jetbrains_linux_xml_credit_scaling_and_selected_ide_window")
    else {
        return;
    };
    let xml = |used, until: &str| {
        let quota =
            json!({"current":used,"maximum":1_000_000,"available":1_000_000-used,"until":until});
        let refill = json!({"next":until,"tariff":{"amount":1_000_000,"duration":"PT720H"}});
        format!(
            "<application><option name=\"quotaInfo\" value=\"{}\"/><option name=\"nextRefill\" value=\"{}\"/></application>",
            quota.to_string().replace('"', "&quot;"),
            refill.to_string().replace('"', "&quot;")
        )
    };
    let older = ".config/JetBrains/IntelliJIdea2025.3/options/AIAssistantQuotaManager2.xml";
    let newer = ".config/JetBrains/WebStorm2026.1/options/AIAssistantQuotaManager2.xml";
    write(&paths, older, &xml(100_000, "2099-01-01T00:00:00Z"));
    write(&paths, newer, &xml(725_000, "2099-02-01T00:00:00Z"));
    let (out, observed) = probe(&paths, "jetbrains-ai-assistant", vec![]);
    labels(&out, &["Quota", "Used", "Remaining"]);
    metric(&out, "Quota", 72.5, 100.0, json!({"kind":"percent"}));
    assert_eq!(line(&out, "Used")["value"], "7.25 / 10 credits");
    assert_eq!(line(&out, "Remaining")["value"], "2.75 credits");
    assert_eq!(line(&out, "Quota")["resetsAt"], "2099-02-01T00:00:00.000Z");
    assert_eq!(line(&out, "Quota")["periodDurationMs"], 2_592_000_000_u64);
    no_writes(&observed);
    write(&paths, older, "<bad/>");
    write(&paths, newer, "<bad/>");
    let (out, _) = probe(&paths, "jetbrains-ai-assistant", vec![]);
    error(
        &out,
        "JetBrains AI Assistant quota data unavailable. Open AI Assistant once and try again.",
    );
    fs::remove_file(paths.root.join(older)).unwrap();
    fs::remove_file(paths.root.join(newer)).unwrap();
    let (out, _) = probe(&paths, "jetbrains-ai-assistant", vec![]);
    error(
        &out,
        "JetBrains AI Assistant not detected. Open a JetBrains IDE with AI Assistant enabled.",
    );
}

fn grok_request(token: &str, settings: bool) -> Value {
    let mut req = request(
        if settings {
            "https://cli-chat-proxy.grok.com/v1/settings"
        } else {
            "https://cli-chat-proxy.grok.com/v1/billing?format=credits"
        },
        token,
        10000,
    );
    req["headers"]["X-XAI-Token-Auth"] = json!("xai-grok-cli");
    req["headers"]["User-Agent"] = json!("OpenUsage");
    req
}

#[test]
fn grok_file_refresh_billing_and_auxiliary_failure() {
    let Some(paths) = hermetic::enter("grok_file_refresh_billing_and_auxiliary_failure") else {
        return;
    };
    let auth = ".grok/auth.json";
    write(
        &paths,
        auth,
        r#"{"https://auth.x.ai::fixture-client":{"key":"old-grok","refresh_token":"grok+refresh","expires_at":"2000-01-01T00:00:00Z"}}"#,
    );
    let refresh = json!({"method":"POST","url":"https://auth.x.ai/oauth2/token",
        "headers":{"Content-Type":"application/x-www-form-urlencoded"},"timeoutMs":15000,
        "bodyText":"grant_type=refresh_token&client_id=fixture-client&refresh_token=grok%2Brefresh"});
    let billing = json!({"config":{"creditUsagePercent":"7.1","currentPeriod":{
        "type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-05-01T00:00:00Z","end":"2099-06-01T00:00:00Z"},"onDemandCap":{"val":"125"}}});
    let (out, observed) = probe(
        &paths,
        "grok",
        vec![
            exchange(
                refresh,
                200,
                json!({"access_token":"new-grok","refresh_token":"rotated-grok","expires_in":3600}),
            ),
            exchange(grok_request("new-grok", false), 200, billing.clone()),
            exchange(
                grok_request("new-grok", true),
                200,
                json!({"subscription_tier_display":"SuperGrok Heavy"}),
            ),
        ],
    );
    labels(&out, &["Weekly limit", "Pay as you go"]);
    metric(&out, "Weekly limit", 7.1, 100.0, json!({"kind":"percent"}));
    assert_eq!(
        line(&out, "Weekly limit")["resetsAt"],
        "2099-06-01T00:00:00.000Z"
    );
    assert_eq!(line(&out, "Pay as you go")["text"], "125 cap");
    assert_eq!(out["plan"], "SuperGrok Heavy");
    let stored: Value =
        serde_json::from_str(&fs::read_to_string(paths.root.join(auth)).unwrap()).unwrap();
    assert_eq!(
        stored["https://auth.x.ai::fixture-client"]["key"],
        "new-grok"
    );
    assert_eq!(
        stored["https://auth.x.ai::fixture-client"]["refresh_token"],
        "rotated-grok"
    );
    assert_eq!(observed["writes"].as_array().unwrap().len(), 1);
    let (out, observed) = probe(
        &paths,
        "grok",
        vec![
            exchange(grok_request("new-grok", false), 200, billing),
            exchange(grok_request("new-grok", true), 503, json!({})),
        ],
    );
    labels(&out, &["Weekly limit", "Pay as you go"]);
    assert!(out["plan"].is_null());
    no_writes(&observed);
    let (out, _) = probe(
        &paths,
        "grok",
        vec![exchange(grok_request("new-grok", false), 200, json!({}))],
    );
    error(&out, "Grok billing response changed.");
}

#[test]
fn ollama_real_cookie_precedence_stale_cookie_and_api_balance() {
    let Some(paths) = hermetic::enter("ollama_real_cookie_precedence_stale_cookie_and_api_balance")
    else {
        return;
    };
    env("OLLAMA_API_KEY", "ollama-key");
    env("OLLAMA_SESSION_COOKIE", "session==");
    env("OLLAMA_COOKIE", "__Secure-session=unselected");
    let req = json!({"method":"GET","url":"https://ollama.com/settings","headers":{
        "Accept":"text/html","Cookie":"__Secure-session=session==","User-Agent":"OpenUsage"},"timeoutMs":10000});
    let html = r#"<main><h1>Cloud Usage <span>Pro</span></h1><section>Session usage <span>0.6% used</span><time data-time="2026-05-16T14:55:00Z"></time></section><section>Weekly usage <span>17.9% used</span><time data-time="2026-05-17T13:00:00Z"></time></section></main>"#;
    let (out, observed) = probe(
        &paths,
        "ollama",
        vec![json!({"request":req,"reply":{"status":200,"headers":{},"bodyText":html}})],
    );
    labels(&out, &["Session", "Weekly", "Source"]);
    metric(&out, "Session", 0.6, 100.0, json!({"kind":"percent"}));
    metric(&out, "Weekly", 17.9, 100.0, json!({"kind":"percent"}));
    assert_eq!(line(&out, "Session")["resetsAt"], "2026-05-16T14:55:00Z");
    assert_eq!(line(&out, "Source")["value"], "Settings page");
    no_writes(&observed);
    let (out, _) = probe(&paths, "ollama", vec![exchange(req, 302, json!({}))]);
    error(&out, "Ollama session expired. Update your session cookie.");
    env("OLLAMA_SESSION_COOKIE", "");
    env("OLLAMA_COOKIE", "");
    let req = request("https://ollama.com/api/balance", "ollama-key", 10000);
    let (out, observed) = probe(
        &paths,
        "ollama",
        vec![exchange(
            req.clone(),
            200,
            json!({"plan":"pro",
        "included":{"balance_usd":"15.6","allowance_usd":"20","period":{"from":"2026-07-01T00:00:00Z","until":"2026-08-01T00:00:00Z"}},
        "purchased":{"balance_usd":12.5}}),
        )],
    );
    labels(&out, &["Monthly", "Purchased Credits", "Source"]);
    metric(&out, "Monthly", 4.4, 20.0, json!({"kind":"dollars"}));
    assert_eq!(line(&out, "Monthly")["periodDurationMs"], 2_678_400_000_u64);
    assert_eq!(line(&out, "Purchased Credits")["value"], "$12.50 remaining");
    assert_eq!(line(&out, "Source")["value"], "Ollama API");
    no_writes(&observed);
    let (out, _) = probe(&paths, "ollama", vec![exchange(req, 401, json!({}))]);
    error(&out, "Ollama API key invalid. Check OLLAMA_API_KEY.");
}

#[test]
fn kiro_linux_profile_live_refresh_preserves_original_token_file() {
    let Some(paths) =
        hermetic::enter("kiro_linux_profile_live_refresh_preserves_original_token_file")
    else {
        return;
    };
    let auth = ".aws/sso/cache/kiro-auth-token.json";
    let arn = "arn:aws:codewhisperer:eu-west-1:123456789012:profile/FIXTURE";
    write(
        &paths,
        auth,
        r#"{"accessToken":"old-kiro","refreshToken":"kiro-refresh","expiresAt":"2000-01-01T00:00:00Z","provider":"Google"}"#,
    );
    write(
        &paths,
        ".config/Kiro/User/globalStorage/kiro.kiroagent/profile.json",
        &json!({"arn":arn}).to_string(),
    );
    let refresh = json!({"method":"POST","url":"https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken",
        "headers":{"Content-Type":"application/json","User-Agent":"OpenUsage/test"},
        "bodyText":r#"{"refreshToken":"kiro-refresh"}"#,"timeoutMs":15000});
    let mut req = request(
        "https://q.eu-west-1.amazonaws.com/getUsageLimits?origin=AI_EDITOR&profileArn=arn%3Aaws%3Acodewhisperer%3Aeu-west-1%3A123456789012%3Aprofile%2FFIXTURE&resourceType=AGENTIC_REQUEST",
        "new-kiro",
        15000,
    );
    req["headers"]["User-Agent"] = json!("OpenUsage/test");
    let (out, observed) = probe(
        &paths,
        "kiro",
        vec![
            exchange(
                refresh.clone(),
                200,
                json!({"accessToken":"new-kiro","refreshToken":"rotated-kiro","expiresIn":3600}),
            ),
            exchange(
                req,
                200,
                json!({"subscriptionInfo":{"subscriptionTitle":"KIRO FREE"},
            "overageConfiguration":{"overageStatus":"DISABLED"},"usageBreakdownList":[{
                "resourceType":"CREDIT","currentUsageWithPrecision":12.25,"usageLimit":50,
                "nextDateReset":"2099-05-01T00:00:00Z","freeTrialInfo":{"currentUsageWithPrecision":106.11,
                    "usageLimit":500,"freeTrialStatus":"ACTIVE","freeTrialExpiry":"2099-05-03T00:00:00Z"}}]}),
            ),
        ],
    );
    labels(&out, &["Credits", "Bonus Credits", "Overages"]);
    metric(
        &out,
        "Credits",
        12.25,
        50.0,
        json!({"kind":"count","suffix":"credits"}),
    );
    metric(
        &out,
        "Bonus Credits",
        106.11,
        500.0,
        json!({"kind":"count","suffix":"credits"}),
    );
    assert_eq!(out["plan"], "Kiro Free");
    assert_eq!(line(&out, "Overages")["text"], "Disabled");
    let stored: Value =
        serde_json::from_str(&fs::read_to_string(paths.root.join(auth)).unwrap()).unwrap();
    assert_eq!(stored["accessToken"], "new-kiro");
    assert_eq!(stored["refreshToken"], "rotated-kiro");
    assert_eq!(stored["provider"], "Google");
    assert_eq!(observed["writes"].as_array().unwrap().len(), 1);
    write(
        &paths,
        auth,
        r#"{"accessToken":"old-kiro","refreshToken":"kiro-refresh","expiresAt":"2000-01-01T00:00:00Z","provider":"Google"}"#,
    );
    let (out, observed) = probe(&paths, "kiro", vec![exchange(refresh, 401, json!({}))]);
    error(&out, "Kiro session expired. Open Kiro and sign in again.");
    no_writes(&observed);
}

#[test]
fn opencode_go_file_key_official_api_metrics_and_auth_failure() {
    let Some(paths) = hermetic::enter("opencode_go_file_key_official_api_metrics_and_auth_failure")
    else {
        return;
    };
    write(
        &paths,
        ".local/share/opencode/auth.json",
        r#"{"opencode-go":{"type":"api-key","key":" go-file "}}"#,
    );
    let req = request("https://opencode.ai/zen/go/v1/usage", "go-file", 15000);
    let (out, observed) = probe(
        &paths,
        "opencode-go",
        vec![exchange(
            req.clone(),
            200,
            json!({"usage":{
        "rolling":{"percent":12.5,"resetsAt":"2099-03-06T17:00:00Z"},
        "weekly":{"percent":40,"resetsAt":"2099-03-09T00:00:00Z"},
        "monthly":{"percent":8,"resetsAt":"2099-04-01T00:00:00Z"}}}),
        )],
    );
    labels(&out, &["Session", "Weekly", "Monthly"]);
    assert_eq!(out["plan"], "Go");
    for (label, used, period) in [
        ("Session", 12.5, 18_000_000),
        ("Weekly", 40.0, 604_800_000),
        ("Monthly", 8.0, 2_592_000_000_u64),
    ] {
        metric(&out, label, used, 100.0, json!({"kind":"percent"}));
        assert_eq!(line(&out, label)["periodDurationMs"], period);
    }
    assert_eq!(
        line(&out, "Session")["sessionStartSignal"],
        "missingResetDate"
    );
    no_writes(&observed);
    for (status, body, message) in [
        (
            401,
            json!({}),
            "OpenCode Go key was rejected. Log into OpenCode Go again.",
        ),
        (
            403,
            json!({"error":{"type":"EntitlementError"}}),
            "No OpenCode Go subscription on this key.",
        ),
        (
            200,
            json!({"usage":{}}),
            "OpenCode Go usage API returned an invalid response.",
        ),
    ] {
        let (out, _) = probe(
            &paths,
            "opencode-go",
            vec![exchange(req.clone(), status, body)],
        );
        error(&out, message);
    }
}

#[test]
fn fireworks_env_quota_fallback_uses_production_builders_without_export_escape() {
    let Some(paths) = hermetic::enter(
        "fireworks_env_quota_fallback_uses_production_builders_without_export_escape",
    ) else {
        return;
    };
    // Backend consumer unit tests supply JS line builders; this covers the ordinary
    // environment/quota branch through the real result parser with export denied.
    env("FIREWORKS_API_KEY", " fireworks-env ");
    let req = |url| {
        let mut req = request(url, "fireworks-env", 15000);
        req["headers"]["User-Agent"] = json!("OpenUsage/test");
        req
    };
    let accounts = req("https://api.fireworks.ai/v1/accounts?pageSize=200");
    let quotas = req("https://api.fireworks.ai/v1/accounts/selected/quotas?pageSize=200");
    let body = json!({"accounts":[{"name":"accounts/unselected","state":"DISABLED"},
        {"name":"accounts/selected","state":"READY","suspendState":"UNSUSPENDED"}]});
    let (out, observed) = probe(
        &paths,
        "fireworks-ai",
        vec![
            exchange(accounts.clone(), 200, body.clone()),
            exchange(
                quotas.clone(),
                200,
                json!({"quotas":[
            {"name":"accounts/selected/quotas/monthly-spend-usd","value":"50","maxValue":"500","usage":"12.34"},
            {"name":"accounts/selected/quotas/serverless-inference-total-tokens","usage":"1250000"}]}),
            ),
        ],
    );
    labels(&out, &["Serverless usage", "Month spend", "Budget"]);
    assert_eq!(out["plan"], "Tier 2");
    assert_eq!(line(&out, "Serverless usage")["value"], "1.25M tokens");
    assert_eq!(line(&out, "Month spend")["value"], "$12.34");
    assert_eq!(line(&out, "Budget")["value"], "$50");
    assert_eq!(line(&out, "Budget")["subtitle"], "Tier cap $500");
    no_writes(&observed);
    let (out, _) = probe(
        &paths,
        "fireworks-ai",
        vec![exchange(accounts.clone(), 401, json!({}))],
    );
    error(&out, "API key invalid. Check your Fireworks AI API key.");
    let (out, _) = probe(
        &paths,
        "fireworks-ai",
        vec![
            exchange(accounts, 200, body),
            exchange(quotas, 500, json!({})),
        ],
    );
    error(&out, "Request failed (HTTP 500). Try again later.");
    assert!(!paths.app_data.join("fireworks").exists());
}

fn claude_request(token: &str, endpoint: &str) -> Value {
    let profile = endpoint == "profile";
    let mut req = request(
        &format!("https://api.anthropic.com/api/oauth/{endpoint}"),
        token,
        10000,
    );
    req["headers"]["anthropic-beta"] = json!("oauth-2025-04-20");
    req["headers"]["User-Agent"] = json!(if profile {
        "claude-code/2.1.69"
    } else {
        "claude-cli/2.1.280 (external, cli)"
    });
    if !profile {
        req["headers"]["Content-Type"] = json!("application/json");
    }
    req
}

fn claude_usage() -> Value {
    json!({"five_hour":{"utilization":23.5,"resets_at":"2099-01-01T00:00:00Z"},
        "seven_day":{"utilization":61,"resets_at":"2099-01-07T00:00:00Z"},
        "extra_usage":{"is_enabled":true,"used_credits":125,"monthly_limit":2000}})
}

#[test]
fn claude_native_file_usage_profile_reset_metadata_and_auxiliary_failures() {
    let Some(paths) =
        hermetic::enter("claude_native_file_usage_profile_reset_metadata_and_auxiliary_failures")
    else {
        return;
    };
    let auth = ".claude/.credentials.json";
    let bytes = json!({"claudeAiOauth":{"accessToken":"claude-file","expiresAt":4102444800000_u64,
        "scopes":["user:profile"],"subscriptionType":"pro"}})
    .to_string();
    write(&paths, auth, &bytes);
    env("CLAUDE_CODE_OAUTH_TOKEN", "inference-only-unselected");
    for auxiliary_ok in [true, false, true] {
        let (out, observed) = probe(
            &paths,
            "claude",
            vec![
                exchange(claude_request("claude-file", "usage"), 200, claude_usage()),
                exchange(
                    claude_request("claude-file", "usage?cedar_ember=1&skip_spend=1"),
                    if auxiliary_ok { 200 } else { 503 },
                    json!({"cedar_ember":{"eligible":true,"grants":[
                    {"resets_left":2,"ends_at":"2099-02-01T00:00:00Z"},
                    {"resets_left":1,"ends_at":"2099-01-01T00:00:00Z"},
                    {"resets_left":7,"ends_at":"2000-01-01T00:00:00Z"}]}}),
                ),
                exchange(
                    claude_request("claude-file", "profile"),
                    if auxiliary_ok { 200 } else { 503 },
                    json!({"organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_5x"}}),
                ),
            ],
        );
        if auxiliary_ok {
            labels(
                &out,
                &[
                    "Session",
                    "Weekly",
                    "Rate Limit Resets",
                    "Extra usage spent",
                ],
            );
            assert_eq!(out["plan"], "Max 5x");
            assert_eq!(line(&out, "Rate Limit Resets")["value"], "3 available");
            assert_eq!(
                line(&out, "Rate Limit Resets")["resetCreditExpiries"],
                json!([
                    "2099-01-01T00:00:00.000Z",
                    "2099-02-01T00:00:00.000Z",
                    "2099-02-01T00:00:00.000Z"
                ])
            );
        } else {
            labels(&out, &["Session", "Weekly", "Extra usage spent"]);
            assert_eq!(out["plan"], "Pro");
        }
        metric(&out, "Session", 23.5, 100.0, json!({"kind":"percent"}));
        metric(&out, "Weekly", 61.0, 100.0, json!({"kind":"percent"}));
        metric(
            &out,
            "Extra usage spent",
            1.25,
            20.0,
            json!({"kind":"dollars"}),
        );
        assert_eq!(
            line(&out, "Session")["sessionStartSignal"],
            "missingResetDate"
        );
        assert_eq!(line(&out, "Session")["periodDurationMs"], 18_000_000);
        assert!(out.get("warning").is_none());
        no_writes(&observed);
        assert_eq!(fs::read_to_string(paths.root.join(auth)).unwrap(), bytes);
    }
    // Each production invocation creates a new JS runtime: the exact three-request
    // queue above repeats, rather than inferring throttling/cache reuse from time.
}

#[test]
fn claude_native_refresh_selected_file_write_and_rejected_requests() {
    let Some(paths) =
        hermetic::enter("claude_native_refresh_selected_file_write_and_rejected_requests")
    else {
        return;
    };
    let auth = ".claude/.credentials.json";
    let original = json!({"sentinel":"preserved","claudeAiOauth":{"accessToken":"old-claude","refreshToken":"refresh-claude",
        "expiresAt":1,"scopes":["user:profile"],"subscriptionType":"pro"}}).to_string();
    write(&paths, auth, &original);
    let refresh = json!({"method":"POST","url":"https://platform.claude.com/v1/oauth/token",
        "headers":{"Content-Type":"application/json"},"timeoutMs":15000,
        "bodyText":r#"{"grant_type":"refresh_token","refresh_token":"refresh-claude","client_id":"9d1c250a-e61b-44d9-88ed-5944d1962f5e","scope":"user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"}"#});
    let (out, observed) = probe(
        &paths,
        "claude",
        vec![
            exchange(
                refresh.clone(),
                200,
                json!({"access_token":"fresh-claude","refresh_token":"rotated-claude","expires_in":3600}),
            ),
            exchange(claude_request("fresh-claude", "usage"), 200, claude_usage()),
            exchange(
                claude_request("fresh-claude", "usage?cedar_ember=1&skip_spend=1"),
                503,
                json!({}),
            ),
            exchange(claude_request("fresh-claude", "profile"), 503, json!({})),
        ],
    );
    metric(&out, "Session", 23.5, 100.0, json!({"kind":"percent"}));
    assert_eq!(observed["writes"].as_array().unwrap().len(), 1);
    assert_eq!(observed["writes"][0]["path"], "~/.claude/.credentials.json");
    let stored: Value =
        serde_json::from_str(&fs::read_to_string(paths.root.join(auth)).unwrap()).unwrap();
    assert_eq!(stored["sentinel"], "preserved");
    assert_eq!(stored["claudeAiOauth"]["accessToken"], "fresh-claude");
    assert_eq!(stored["claudeAiOauth"]["refreshToken"], "rotated-claude");
    write(&paths, auth, &original);
    let (out, observed) = probe(
        &paths,
        "claude",
        vec![exchange(refresh, 401, json!({"error":"invalid_grant"}))],
    );
    error(&out, "Session expired. Run `claude` to log in again.");
    no_writes(&observed);
    assert_eq!(fs::read_to_string(paths.root.join(auth)).unwrap(), original);
    write(&paths,auth,&json!({"claudeAiOauth":{"accessToken":"valid-claude","expiresAt":4102444800000_u64,"scopes":["user:profile"]}}).to_string());
    for (status, body, message) in [
        (
            401,
            json!({}),
            "Token expired. Run `claude` to log in again.",
        ),
        (200, Value::Null, "Usage response invalid. Try again later."),
    ] {
        let (out, observed) = probe(
            &paths,
            "claude",
            vec![exchange(
                claude_request("valid-claude", "usage"),
                status,
                body,
            )],
        );
        error(&out, message);
        no_writes(&observed);
    }
}

#[test]
fn claude_native_rate_limit_warning_survives_typed_output() {
    let Some(paths) = hermetic::enter("claude_native_rate_limit_warning_survives_typed_output")
    else {
        return;
    };
    write(
        &paths,
        ".claude/.credentials.json",
        &json!({"claudeAiOauth":{"accessToken":"limited-claude",
        "expiresAt":4102444800000_u64,"scopes":["user:profile"]}})
        .to_string(),
    );
    let (out, observed) = probe(
        &paths,
        "claude",
        vec![json!({"request":claude_request("limited-claude","usage"),
        "reply":{"status":429,"headers":{"retry-after":"120"},"bodyText":"{}"}})],
    );
    labels(&out, &["Status", "Note"]);
    assert_eq!(
        out["warning"],
        "Updates blocked by Anthropic — retry in ~2m"
    );
    assert_eq!(line(&out, "Status")["text"], "Rate limited, retry in ~2m");
    assert_eq!(
        line(&out, "Note")["value"],
        "Live usage rate limited — retry in ~2m"
    );
    no_writes(&observed);
}

fn reader(method: &str, args: Value, value: Value) -> Value {
    json!({"call":{"method":method,"args":args},"value":value})
}

fn ls_discovery(id: &str) -> Value {
    reader(
        "ls.discover",
        json!([{"processName":"language_server",
        "markers":if id == "antigravity" {vec!["antigravity","antigravity-ide"]} else {vec!["antigravity-ide"]},
        "csrfFlag":"--csrf_token","portFlag":"--extension_server_port"}]),
        json!({"pid":12345,"ports":[42001],"extensionPort":null,"csrf":format!("{id}-csrf")}),
    )
}

fn ls_request(id: &str, method: &str, tls: bool) -> Value {
    let body = if method == "GetUnleashData" {
        json!({"context":{"properties":{"devMode":"false","extensionVersion":"unknown",
            "ide":id,"ideVersion":"unknown","os":if id == "antigravity" {"macos"} else {"linux"}}}})
    } else {
        json!({"metadata":{"ideName":id,"extensionName":id,"ideVersion":"unknown","locale":"en"}})
    };
    // Wire JSON field order is part of the expected request body below.
    let body_text = if method == "GetUnleashData" {
        format!(
            r#"{{"context":{{"properties":{{"devMode":"false","extensionVersion":"unknown","ide":"{id}","ideVersion":"unknown","os":"{}"}}}}}}"#,
            if id == "antigravity" {
                "macos"
            } else {
                "linux"
            }
        )
    } else {
        format!(
            r#"{{"metadata":{{"ideName":"{id}","extensionName":"{id}","ideVersion":"unknown","locale":"en"}}}}"#
        )
    };
    assert_eq!(serde_json::from_str::<Value>(&body_text).unwrap(), body);
    json!({"method":"POST","url":format!("{}://127.0.0.1:42001/exa.language_server_pb.LanguageServerService/{method}",if tls {"https"} else {"http"}),
        "headers":{"Content-Type":"application/json","Connect-Protocol-Version":"1","x-codeium-csrf-token":format!("{id}-csrf")},
        "bodyText":body_text,"timeoutMs":if method == "GetUnleashData" {5000} else {10000},"dangerouslyIgnoreTls":tls})
}

fn model_configs() -> Value {
    json!([
        {"label":"Gemini 3 Pro (High)","modelOrAlias":{"model":"fixture-pro"},"quotaInfo":{"remainingFraction":0.8,"resetTime":"2099-01-01T00:00:00Z"}},
        {"label":"Gemini 3 Pro (Low)","modelOrAlias":{"model":"fixture-pro-low"},"quotaInfo":{"remainingFraction":0.65,"resetTime":"2099-01-02T00:00:00Z"}},
        {"label":"Claude Sonnet 4.6","modelOrAlias":{"model":"fixture-claude"},"quotaInfo":{"remainingFraction":0.4,"resetTime":"2099-01-03T00:00:00Z"}}
    ])
}

#[test]
fn antigravity_controlled_ls_caller_summary_and_auxiliary_failure() {
    let Some(paths) =
        hermetic::enter("antigravity_controlled_ls_caller_summary_and_auxiliary_failure")
    else {
        return;
    };
    for summary_ok in [true, false] {
        let (out, observed) = probe_with_readers(
            &paths,
            "antigravity",
            vec![
                json!({"request":ls_request("antigravity","GetUnleashData",true),"networkError":true}),
                exchange(
                    ls_request("antigravity", "GetUnleashData", false),
                    400,
                    json!({}),
                ),
                exchange(
                    ls_request("antigravity", "RetrieveUserQuotaSummary", false),
                    if summary_ok { 200 } else { 503 },
                    json!({"groups":[{"buckets":[{"bucketId":"gemini-5h","remainingFraction":0.75,"resetTime":"2099-01-01T00:00:00Z"},
                    {"bucketId":"3p-weekly","remainingFraction":0.6,"resetTime":"2099-01-07T00:00:00Z"}]}]}),
                ),
                exchange(
                    ls_request("antigravity", "GetUserStatus", false),
                    200,
                    json!({"userStatus":{
                "userTier":{"name":"Google AI Ultra"},"planStatus":{"planInfo":{"planName":"unselected-plan"}},
                "cascadeModelConfigData":{"clientModelConfigs":model_configs()}}}),
                ),
            ],
            Some(vec![ls_discovery("antigravity")]),
        );
        assert_eq!(out["plan"], "Google AI Ultra");
        if summary_ok {
            labels(&out, &["Session", "Weekly — Claude and GPT Models"]);
            metric(&out, "Session", 25.0, 100.0, json!({"kind":"percent"}));
            metric(
                &out,
                "Weekly — Claude and GPT Models",
                40.0,
                100.0,
                json!({"kind":"percent"}),
            );
            assert_eq!(
                line(&out, "Weekly — Claude and GPT Models")["periodDurationMs"],
                604_800_000
            );
        } else {
            labels(&out, &["Gemini Pro", "Claude"]);
            metric(&out, "Gemini Pro", 35.0, 100.0, json!({"kind":"percent"}));
            metric(&out, "Claude", 60.0, 100.0, json!({"kind":"percent"}));
        }
        no_writes(&observed);
    }
}

#[test]
fn antigravity_ide_controlled_ls_caller_tls_and_method_fallback() {
    let Some(paths) =
        hermetic::enter("antigravity_ide_controlled_ls_caller_tls_and_method_fallback")
    else {
        return;
    };
    for user_status_ok in [true, false] {
        let mut exchanges = vec![
            json!({"request":ls_request("antigravity-ide","GetUnleashData",true),"networkError":true}),
            exchange(
                ls_request("antigravity-ide", "GetUnleashData", false),
                400,
                json!({}),
            ),
            exchange(
                ls_request("antigravity-ide", "GetUserStatus", false),
                if user_status_ok { 200 } else { 503 },
                json!({"userStatus":{"userTier":{"name":"IDE Ultra"},"cascadeModelConfigData":{"clientModelConfigs":model_configs()}}}),
            ),
        ];
        if !user_status_ok {
            exchanges.push(exchange(
                ls_request("antigravity-ide", "GetCommandModelConfigs", false),
                200,
                json!({"clientModelConfigs":model_configs()}),
            ));
        }
        let (out, observed) = probe_with_readers(
            &paths,
            "antigravity-ide",
            exchanges,
            Some(vec![ls_discovery("antigravity-ide")]),
        );
        labels(&out, &["Gemini Pro", "Claude"]);
        metric(&out, "Gemini Pro", 35.0, 100.0, json!({"kind":"percent"}));
        metric(&out, "Claude", 60.0, 100.0, json!({"kind":"percent"}));
        assert_eq!(line(&out, "Gemini Pro")["resetsAt"], "2099-01-02T00:00:00Z");
        assert_eq!(
            out["plan"],
            if user_status_ok {
                json!("IDE Ultra")
            } else {
                Value::Null
            }
        );
        no_writes(&observed);
    }
    let (out, _) = probe_with_readers(
        &paths,
        "antigravity-ide",
        vec![],
        Some(vec![reader(
            "ls.discover",
            ls_discovery("antigravity-ide")["call"]["args"].clone(),
            Value::Null,
        )]),
    );
    error(&out, "Start Antigravity IDE and try again.");
}

fn sqlite_text_rows(db: &Path, sql: &str) -> Value {
    let connection = rusqlite::Connection::open(db).unwrap();
    let mut statement = connection.prepare(sql).unwrap();
    let columns: Vec<String> = statement
        .column_names()
        .map(|name| name.to_string())
        .collect();
    let rows = statement
        .query_map([], |row| {
            let mut value = serde_json::Map::new();
            for (i, name) in columns.iter().enumerate() {
                value.insert(name.clone(), json!(row.get::<_, String>(i)?));
            }
            Ok(Value::Object(value))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    json!(rows)
}

fn cursor_jwt(subject: &str) -> String {
    format!(
        "e30.{}.fixture",
        URL_SAFE_NO_PAD
            .encode(json!({"sub":format!("auth|{subject}"),"exp":4102444800_u64}).to_string())
    )
}

fn cursor_connect(token: &str, method: &str) -> Value {
    json!({"method":"POST","url":format!("https://api2.cursor.sh/aiserver.v1.DashboardService/{method}"),
        "headers":{"Authorization":format!("Bearer {token}"),"Content-Type":"application/json",
            "Connect-Protocol-Version":"1","Accept":"application/json","User-Agent":"Cursor/1.0.0"},
        "bodyText":"{}","timeoutMs":10000})
}

fn cursor_controlled_sqlite_case(paths: &HermeticPaths, id: &str) {
    for (app, subject) in [
        ("Cursor", "standard-selected"),
        ("Cursor Nightly", "nightly-selected"),
    ] {
        let path = paths
            .root
            .join(format!(".config/{app}/User/globalStorage/state.vscdb"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .execute_batch("CREATE TABLE ItemTable(key TEXT PRIMARY KEY,value TEXT);")
            .unwrap();
        connection
            .execute(
                "INSERT INTO ItemTable VALUES ('cursorAuth/accessToken',?1)",
                [cursor_jwt(subject)],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO ItemTable VALUES ('cursorAuth/stripeMembershipType','pro')",
                [],
            )
            .unwrap();
    }
    let (app, subject) = if id == "cursor" {
        ("Cursor", "standard-selected")
    } else {
        ("Cursor Nightly", "nightly-selected")
    };
    let db = paths
        .root
        .join(format!(".config/{app}/User/globalStorage/state.vscdb"));
    let token = cursor_jwt(subject);
    let mut readers = vec![];
    for key in [
        "cursorAuth/accessToken",
        "cursorAuth/refreshToken",
        "cursorAuth/stripeMembershipType",
    ] {
        let sql = format!("SELECT value FROM ItemTable WHERE key = '{key}' LIMIT 1;");
        let rows = sqlite_text_rows(&db, &sql);
        readers.push(reader(
            "sqlite.query",
            json!([db.to_string_lossy(), sql]),
            json!(rows.to_string()),
        ));
    }
    readers.push(reader(
        "keychain.readGenericPassword",
        json!(["cursor-access-token"]),
        json!(cursor_jwt("unselected-keychain")),
    ));
    readers.push(reader(
        "keychain.readGenericPassword",
        json!(["cursor-refresh-token"]),
        Value::Null,
    ));
    let stripe = json!({"method":"GET","url":"https://cursor.com/api/auth/stripe","timeoutMs":10000,
        "headers":{"Authorization":format!("Bearer {token}"),"Cookie":format!("WorkosCursorSessionToken={subject}%3A%3A{token}"),
            "Accept":"application/json","Origin":"https://cursor.com","Referer":"https://cursor.com/dashboard",
            "User-Agent":"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"}});
    let before = fs::read(&db).unwrap();
    for auxiliary_ok in [true, false] {
        let (out, observed) = probe_with_readers(
            paths,
            id,
            vec![
                exchange(
                    cursor_connect(&token, "GetCurrentPeriodUsage"),
                    200,
                    json!({"enabled":true,
                "billingCycleStart":1735689600000_u64,"billingCycleEnd":1738368000000_u64,
                "planUsage":{"limit":2000,"totalSpend":500,"autoPercentUsed":10,"apiPercentUsed":40},
                "spendLimitUsage":{"individualLimit":1000,"individualRemaining":750}}),
                ),
                exchange(
                    cursor_connect(&token, "GetPlanInfo"),
                    200,
                    json!({"planInfo":{"planName":"pro"}}),
                ),
                exchange(
                    cursor_connect(&token, "GetCreditGrantsBalance"),
                    if auxiliary_ok { 200 } else { 503 },
                    json!({"hasCreditGrants":true,"totalCents":"1000","usedCents":"250"}),
                ),
                exchange(
                    stripe.clone(),
                    if auxiliary_ok { 200 } else { 503 },
                    json!({"customerBalance":-500}),
                ),
                exchange(
                    cursor_connect(&token, "GetSandUsageStatus"),
                    if auxiliary_ok { 200 } else { 503 },
                    json!({"usagePercent":"12.5","currentPeriodStart":"2099-01-01T00:00:00Z","nextResetTimestampUtc":"2099-01-08T00:00:00Z"}),
                ),
            ],
            Some(readers.clone()),
        );
        if auxiliary_ok {
            labels(
                &out,
                &[
                    "Credits",
                    "Total usage",
                    "Cursor Models",
                    "Other Models",
                    "Grok Bot usage",
                    "On-demand",
                ],
            );
            metric(&out, "Credits", 2.5, 15.0, json!({"kind":"dollars"}));
            metric(
                &out,
                "Grok Bot usage",
                12.5,
                100.0,
                json!({"kind":"percent"}),
            );
        } else {
            labels(
                &out,
                &["Total usage", "Cursor Models", "Other Models", "On-demand"],
            );
        }
        assert_eq!(out["plan"], "Pro");
        metric(&out, "Total usage", 25.0, 100.0, json!({"kind":"percent"}));
        metric(
            &out,
            "Cursor Models",
            10.0,
            100.0,
            json!({"kind":"percent"}),
        );
        metric(&out, "Other Models", 40.0, 100.0, json!({"kind":"percent"}));
        metric(&out, "On-demand", 2.5, 10.0, json!({"kind":"dollars"}));
        assert_eq!(
            line(&out, "Total usage")["resetsAt"],
            "2025-02-01T00:00:00.000Z"
        );
        assert_eq!(
            line(&out, "Total usage")["periodDurationMs"],
            2_678_400_000_u64
        );
        no_writes(&observed);
        assert_eq!(fs::read(&db).unwrap(), before);
    }
    let (out, observed) = probe_with_readers(
        paths,
        id,
        vec![exchange(
            cursor_connect(&token, "GetCurrentPeriodUsage"),
            500,
            json!({}),
        )],
        Some(readers),
    );
    error(&out, "Usage request failed (HTTP 500). Try again later.");
    no_writes(&observed);
}

#[test]
fn cursor_controlled_sqlite_native_caller_selected_identity_and_quotas() {
    let Some(paths) =
        hermetic::enter("cursor_controlled_sqlite_native_caller_selected_identity_and_quotas")
    else {
        return;
    };
    cursor_controlled_sqlite_case(&paths, "cursor");
}

#[test]
fn cursor_nightly_controlled_sqlite_native_caller_selected_identity_and_quotas() {
    let Some(paths) = hermetic::enter(
        "cursor_nightly_controlled_sqlite_native_caller_selected_identity_and_quotas",
    ) else {
        return;
    };
    cursor_controlled_sqlite_case(&paths, "cursor-nightly");
}

#[test]
fn perplexity_controlled_sqlite_native_cache_no_group_slash_and_failure() {
    let Some(paths) =
        hermetic::enter("perplexity_controlled_sqlite_native_cache_no_group_slash_and_failure")
    else {
        return;
    };
    let relative = ".config/Perplexity/Cache/Cache.db";
    let db = paths.root.join(relative);
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE cfurl_cache_response(entry_ID INTEGER PRIMARY KEY,request_key TEXT);
        CREATE TABLE cfurl_cache_blob_data(entry_ID INTEGER PRIMARY KEY,request_object BLOB);",
        )
        .unwrap();
    for (id, token) in [(1, "old.unselected.token"), (2, "selected.cache.token")] {
        connection
            .execute(
                "INSERT INTO cfurl_cache_response VALUES (?1,'https://www.perplexity.ai/api/user')",
                [id],
            )
            .unwrap();
        let request = format!("Bearer {token} \0Ask/1.2/macOS\0macos:fixture-device\0");
        connection
            .execute(
                "INSERT INTO cfurl_cache_blob_data VALUES (?1,?2)",
                rusqlite::params![id, request.as_bytes()],
            )
            .unwrap();
    }
    drop(connection);
    let sql = "SELECT hex(b.request_object) AS requestHex FROM cfurl_cache_response r JOIN cfurl_cache_blob_data b ON b.entry_ID = r.entry_ID WHERE r.request_key = 'https://www.perplexity.ai/api/user' ORDER BY r.entry_ID DESC LIMIT 1;";
    let rows = sqlite_text_rows(&db, sql);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    let readers = vec![reader(
        "sqlite.query",
        json!(["~/.config/Perplexity/Cache/Cache.db", sql]),
        json!(rows.to_string()),
    )];
    let req = |endpoint: &str| {
        json!({"method":"GET","url":format!("https://www.perplexity.ai{endpoint}"),"timeoutMs":10000,
            "headers":{"Authorization":"Bearer selected.cache.token","Accept":"*/*","User-Agent":"Ask/1.2/macOS",
                "X-Client-Name":"Perplexity-Mac","X-App-ApiVersion":"2.17","X-App-ApiClient":"macos",
                "X-Client-Env":"production","X-App-Version":"1.2","X-Device-ID":"macos:fixture-device"}})
    };
    let before = fs::read(&db).unwrap();
    let (out, observed) = probe_with_readers(
        &paths,
        "perplexity",
        vec![
            exchange(req("/api/user"), 404, json!({})),
            exchange(req("/api/user/"), 200, json!({"subscription_tier":"pro"})),
            exchange(req("/rest/pplx-api/v2/groups"), 200, json!([])),
            exchange(
                req("/rest/rate-limit/all"),
                200,
                json!({"remaining_pro":198,"remaining_research":20,"remaining_labs":25}),
            ),
        ],
        Some(readers.clone()),
    );
    labels(&out, &["Queries", "Deep Research", "Labs"]);
    assert_eq!(out["plan"], "Pro");
    assert_eq!(line(&out, "Queries")["value"], "198 remaining");
    assert_eq!(line(&out, "Deep Research")["value"], "20 remaining");
    assert_eq!(line(&out, "Labs")["value"], "25 remaining");
    no_writes(&observed);
    let (out, observed) = probe_with_readers(
        &paths,
        "perplexity",
        vec![
            exchange(req("/api/user"), 503, json!({})),
            exchange(req("/api/user/"), 503, json!({})),
            exchange(req("/rest/pplx-api/v2/groups"), 503, json!({})),
            exchange(req("/rest/pplx-api/v2/groups/"), 503, json!({})),
            exchange(req("/rest/rate-limit/all"), 503, json!({})),
        ],
        Some(readers),
    );
    error(&out, "Unable to connect. Try again later.");
    no_writes(&observed);
    assert_eq!(fs::read(db).unwrap(), before);
}
