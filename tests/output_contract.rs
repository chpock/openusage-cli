#[path = "support/hermetic.rs"]
mod hermetic;

use hermetic::HermeticPaths;
use openusage_cli::config;
use openusage_cli::plugin_engine::{manifest, runtime};
use serde_json::{Value, json};
use std::fs;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

fn plugin(script: &str) -> manifest::LoadedPlugin {
    manifest::LoadedPlugin {
        manifest: manifest::PluginManifest {
            schema_version: 1,
            id: "output-contract".into(),
            name: "Output Contract".into(),
            version: "0.0.0".into(),
            entry: "plugin.js".into(),
            icon: "icon.svg".into(),
            brand_color: None,
            lines: vec![],
            links: vec![],
        },
        plugin_dir: ".".into(),
        entry_script: script.into(),
        icon_data_url: "data:image/svg+xml;base64,".into(),
    }
}

fn probe(paths: &HermeticPaths, body: &str) -> Value {
    let script = format!("globalThis.__openusage_plugin = {{ probe(ctx) {{ {body} }} }};");
    let result = runtime::run_probe_in_sandbox(
        &plugin(&script),
        &paths.app_data,
        "test",
        Some(&paths.overrides),
        &paths.root,
    );
    assert_eq!(result.outputs.len(), 1);
    serde_json::to_value(&result.outputs[0]).expect("serialize production output")
}

const METRICS: &str = r##"
    return { warning: "Estimated usage", plan: "Test plan", lines: [
        ctx.line.text({label: "Today", value: "$1.25", modelBreakdown: [
            {model: "a", tokens: 75000, costUsd: 1, percent: 75},
            {model: "b", tokens: 25000, percent: 25}
        ], statusDot: "warning", expiryTooltip: "Two credits expire soon",
        resetCreditExpiries: ["2026-10-25T00:00:00Z", "2026-10-25T00:00:00Z"]}),
        ctx.line.progress({label: "Session", used: 0, limit: 100,
            format: {kind: "percent"}, sessionStartSignal: "missingResetDate"}),
        ctx.line.progress({label: "Spend", used: 12.5, limit: 100,
            format: {kind: "currency", currency: "CNY"}}),
        ctx.line.progress({label: "USD", used: 1.25, limit: 10, format: {kind: "dollars"}}),
        ctx.line.progress({label: "Requests", used: 1.5, limit: 10,
            format: {kind: "count", suffix: "requests"}}),
        ctx.line.barChart({label: "Trend", points: [
            {label: "Oct 24", value: 0}, {label: "Oct 24", value: 2400, valueLabel: "2.4K tokens"}
        ], note: "Local logs", color: "#123456"}),
        ctx.line.text({label: "Account", value: "test"}),
        ctx.line.badge({label: "Status", text: "Connected"})
    ] };
"##;

fn expected_lines() -> Value {
    json!([
        {"type":"text", "label":"Today", "value":"$1.25", "color":null, "subtitle":null,
         "modelBreakdown":[{"model":"a", "tokens":75000, "costUsd":1.0, "percent":75.0},
                           {"model":"b", "tokens":25000, "percent":25.0}],
         "statusDot":"warning", "expiryTooltip":"Two credits expire soon",
         "resetCreditExpiries":["2026-10-25T00:00:00Z", "2026-10-25T00:00:00Z"]},
        {"type":"progress", "label":"Session", "used":0.0, "limit":100.0,
         "format":{"kind":"percent"}, "resetsAt":null, "periodDurationMs":null, "color":null,
         "sessionStartSignal":"missingResetDate"},
        {"type":"progress", "label":"Spend", "used":12.5, "limit":100.0,
         "format":{"kind":"currency", "currency":"CNY"}, "resetsAt":null, "periodDurationMs":null, "color":null},
        {"type":"progress", "label":"USD", "used":1.25, "limit":10.0,
         "format":{"kind":"dollars"}, "resetsAt":null, "periodDurationMs":null, "color":null},
        {"type":"progress", "label":"Requests", "used":1.5, "limit":10.0,
         "format":{"kind":"count", "suffix":"requests"}, "resetsAt":null, "periodDurationMs":null, "color":null},
        {"type":"barChart", "label":"Trend", "points":[{"label":"Oct 24", "value":0.0},
            {"label":"Oct 24", "value":2400.0, "valueLabel":"2.4K tokens"}], "note":"Local logs", "color":"#123456"},
        {"type":"text", "label":"Account", "value":"test", "color":null, "subtitle":null},
        {"type":"badge", "label":"Status", "text":"Connected", "color":null, "subtitle":null}
    ])
}

fn assert_error(line: &Value) {
    assert_eq!(line["type"], "badge", "{line}");
    assert_eq!(line["label"], "Error", "{line}");
    assert!(!line["text"].as_str().expect("error text").is_empty());
}

#[test]
fn output_contract_builders_parser_exact_json() {
    let Some(paths) = hermetic::enter("output_contract_builders_parser_exact_json") else {
        return;
    };
    let output = probe(&paths, METRICS);
    assert_eq!(output["lines"], expected_lines());
    assert_eq!(output["warning"], "Estimated usage");
    assert_eq!(
        output["account"],
        json!({"id":"default", "origin":"native", "isActive":true})
    );
    let decoded: runtime::PluginOutput =
        serde_json::from_value(output.clone()).expect("deserialize new shapes");
    assert_eq!(serde_json::to_value(decoded).unwrap(), output);
}

#[test]
fn output_contract_absent_metadata_keeps_legacy_nulls() {
    let Some(paths) = hermetic::enter("output_contract_absent_metadata_keeps_legacy_nulls") else {
        return;
    };
    let output = probe(
        &paths,
        r#"return {lines: [ctx.line.text({label:"Text", value:"ok"}),
        ctx.line.progress({label:"Session", used:0, limit:100, format:{kind:"percent"}}),
        ctx.line.barChart({label:"Zero", points:[{label:"day", value:0}]})]};"#,
    );
    assert!(output.get("warning").is_none());
    assert_eq!(
        output["lines"],
        json!([
            {"type":"text", "label":"Text", "value":"ok", "color":null, "subtitle":null},
            {"type":"progress", "label":"Session", "used":0.0, "limit":100.0,
             "format":{"kind":"percent"}, "resetsAt":null, "periodDurationMs":null, "color":null},
            {"type":"barChart", "label":"Zero", "points":[{"label":"day", "value":0.0}]}
        ])
    );
}

#[test]
fn output_contract_chart_invalid_points_and_bounded_scan() {
    let Some(paths) = hermetic::enter("output_contract_chart_invalid_points_and_bounded_scan")
    else {
        return;
    };
    let output = probe(
        &paths,
        r#"
        var points = [null, {label:" ", value:1}, {label:"negative", value:-1},
            {label:"nan", value:NaN}, {label:"infinite", value:Infinity},
            {label:"string", value:"1"}, {label:"missing"}];
        while (points.length < 366) points.push({label:" duplicate ", value:0, valueLabel:" "});
        points.push({ get label() { throw Error("must not inspect point 367"); } });
        return {lines:[ctx.line.barChart({label:"Bounded", points:points})]};
    "#,
    );
    let lines = output["lines"].as_array().unwrap();
    assert_eq!(
        lines.len(),
        8,
        "seven invalid points produce badges plus a valid chart"
    );
    for line in &lines[..7] {
        assert_error(line);
    }
    assert_eq!(lines[7]["type"], "barChart");
    let points = lines[7]["points"].as_array().unwrap();
    assert_eq!(
        points.len(),
        359,
        "scan first 366 inputs, not first 366 valid points"
    );
    assert!(
        points
            .iter()
            .all(|point| point == &json!({"label":"duplicate", "value":0.0}))
    );
}

#[test]
fn output_contract_chart_empty_invalid_and_missing_are_errors() {
    let Some(paths) = hermetic::enter("output_contract_chart_empty_invalid_and_missing_are_errors")
    else {
        return;
    };
    for points in ["[]", "[{label:'bad',value:-1}]", "null", "'bad'"] {
        let output = probe(
            &paths,
            &format!("return {{lines:[{{type:'barChart',label:'Trend',points:{points}}}]}};"),
        );
        let lines = output["lines"].as_array().unwrap();
        assert!(!lines.is_empty());
        for line in lines {
            assert_error(line);
        }
    }
    let output = probe(&paths, "return {lines:[{type:'barChart',label:'Trend'}]};");
    assert_error(&output["lines"][0]);
}

#[test]
fn output_contract_currency_rejects_bad_codes_without_changing_units() {
    let Some(paths) =
        hermetic::enter("output_contract_currency_rejects_bad_codes_without_changing_units")
    else {
        return;
    };
    for currency in [
        "undefined",
        "null",
        "12",
        "''",
        "'cny'",
        "' CNY '",
        "'CN'",
        "'CNYY'",
        "'$'",
        "'C1Y'",
    ] {
        let output = probe(
            &paths,
            &format!(
                "return {{lines:[ctx.line.progress({{label:'Spend',used:12.5,limit:100,format:{{kind:'currency',currency:{currency}}}}})]}};"
            ),
        );
        assert_error(&output["lines"][0]);
    }
    let output = probe(
        &paths,
        "return {lines:[ctx.line.progress({label:'Spend',used:12.5,limit:100,format:{kind:'currency',currency:'XYZ'}})]};",
    );
    assert_eq!(
        output["lines"][0]["format"],
        json!({"kind":"currency", "currency":"XYZ"})
    );
    assert_eq!(output["lines"][0]["used"], 12.5);
}

#[test]
fn output_contract_warning_does_not_bypass_empty_result_or_suppression() {
    let Some(paths) =
        hermetic::enter("output_contract_warning_does_not_bypass_empty_result_or_suppression")
    else {
        return;
    };
    for body in [
        "return {warning:'Partial',lines:[]};",
        "return {warning:'Partial'};",
    ] {
        let output = probe(&paths, body);
        assert_eq!(output["warning"], "Partial");
        assert_error(&output["lines"][0]);
    }
    let mut provider = plugin(
        r#"globalThis.__openusage_plugin = {
        discoverAccounts() { return [{id:'default',errorPolicy:'hide-if-other-account'}, {id:'remote',origin:'opencode'}]; },
        probe(ctx) { return {warning:ctx.account.id,lines:[]}; }
    };"#,
    );
    let result =
        runtime::run_probe_in_sandbox(&provider, &paths.app_data, "test", None, &paths.root);
    let outputs = serde_json::to_value(result.outputs).unwrap();
    assert_eq!(outputs.as_array().unwrap().len(), 1);
    assert_eq!(outputs[0]["account"]["id"], "remote");
    assert_eq!(outputs[0]["warning"], "remote");
    assert_error(&outputs[0]["lines"][0]);
    provider.entry_script =
        "globalThis.__openusage_plugin={probe(){throw Error('Failed')}};".into();
    let result =
        runtime::run_probe_in_sandbox(&provider, &paths.app_data, "test", None, &paths.root);
    assert!(
        serde_json::to_value(&result.outputs[0])
            .unwrap()
            .get("warning")
            .is_none()
    );
}

#[test]
fn output_contract_optional_metadata_normalization() {
    let Some(paths) = hermetic::enter("output_contract_optional_metadata_normalization") else {
        return;
    };
    let output = probe(
        &paths,
        r#"return {warning:" ",lines:[ctx.line.text({label:"Details",value:"ok",
        modelBreakdown:[{model:"fraction",tokens:3.9,percent:12.5,costUsd:Infinity},
            {model:"negative",tokens:-4,percent:NaN}, {model:"missing"},
            {model:" ",tokens:100,percent:100}],
        resetCreditExpiries:[" 2026-10-25T00:00:00Z ", "bad", 42, "", null,
            "2026-10-25T00:00:00Z", "2026-10-25T03:00:00+03:00", "2026-10-25T00:00:00"],
        statusDot:"futureSeverity", expiryTooltip:"Credits"}),
        ctx.line.text({label:"Invalid",value:"still valid", modelBreakdown:[],
            resetCreditExpiries:["bad", "2026-10-25", "2026-10-25T00:00:00"]}),
        ctx.line.progress({label:"Session",used:0,limit:100,format:{kind:"percent"},sessionStartSignal:"zeroUsage"})]};"#,
    );
    assert!(output.get("warning").is_none());
    assert_eq!(
        output["lines"][0],
        json!({"type":"text", "label":"Details", "value":"ok", "color":null,
        "subtitle":null, "modelBreakdown":[{"model":"fraction", "tokens":3, "percent":12.5},
            {"model":"negative", "tokens":0, "percent":0.0}, {"model":"missing", "tokens":0, "percent":0.0}],
        "resetCreditExpiries":["2026-10-25T00:00:00Z", "2026-10-25T00:00:00Z", "2026-10-25T03:00:00+03:00"],
        "statusDot":"futureSeverity", "expiryTooltip":"Credits"})
    );
    assert_eq!(
        output["lines"][1],
        json!({"type":"text", "label":"Invalid", "value":"still valid", "color":null, "subtitle":null})
    );
    assert_eq!(output["lines"][2]["sessionStartSignal"], "zeroUsage");
}

#[test]
fn output_contract_chart_caps_valid_points_and_omits_invalid_optional_values() {
    let Some(paths) = hermetic::enter(
        "output_contract_chart_caps_valid_points_and_omits_invalid_optional_values",
    ) else {
        return;
    };
    let output = probe(
        &paths,
        r#"
        var points = [];
        for (var i=0;i<366;i++) points.push({label:String(i),value:i,valueLabel:null});
        points.push({label:"extra",value:1});
        return {lines:[{type:"barChart",label:"Year",points:points,note:42},
            ctx.line.barChart({label:"Optional",points:[{label:"zero",value:0,valueLabel:42}],note:" "})]};
    "#,
    );
    let points = output["lines"][0]["points"].as_array().unwrap();
    assert_eq!(points.len(), 366);
    for (index, point) in points.iter().enumerate() {
        assert_eq!(
            point,
            &json!({"label":index.to_string(), "value":index as f64})
        );
    }
    assert!(output["lines"][0].get("note").is_none());
    assert!(output["lines"][0].get("color").is_none());
    assert_eq!(
        output["lines"][1],
        json!({"type":"barChart","label":"Optional","points":[{"label":"zero","value":0.0}]})
    );
}

#[test]
fn output_contract_warnings_are_per_account_and_probe_identity_stays_host_owned() {
    let Some(paths) = hermetic::enter(
        "output_contract_warnings_are_per_account_and_probe_identity_stays_host_owned",
    ) else {
        return;
    };
    let provider = plugin(
        r#"globalThis.__openusage_plugin={
        discoverAccounts(){return [{id:"default"},{id:"remote",origin:"opencode",name:"Work",isActive:false}]},
        probe(ctx){return {warning:ctx.account.id,account:{id:"spoof",origin:"spoof"},
            lines:[ctx.line.text({label:"Status",value:"ok"})]}}
    };"#,
    );
    let result =
        runtime::run_probe_in_sandbox(&provider, &paths.app_data, "test", None, &paths.root);
    let outputs = serde_json::to_value(result.outputs).unwrap();
    assert_eq!(outputs.as_array().unwrap().len(), 2);
    assert_eq!(outputs[0]["warning"], "default");
    assert_eq!(outputs[1]["warning"], "remote");
    assert_eq!(
        outputs[0]["account"],
        json!({"id":"default", "origin":"native", "isActive":true})
    );
    assert_eq!(
        outputs[1]["account"],
        json!({"id":"remote", "origin":"opencode", "name":"Work", "isActive":false})
    );
}

struct ProbeCounter {
    url: String,
    calls: Arc<AtomicUsize>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ProbeCounter {
    fn start() -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    ready_tx
                        .send(format!("http://{}", listener.local_addr().unwrap()))
                        .unwrap();
                    let app = axum::Router::new().route(
                        "/",
                        axum::routing::get(move || {
                            observed.fetch_add(1, Ordering::SeqCst);
                            async { "ok" }
                        }),
                    );
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = stopped.await;
                        })
                        .await
                        .unwrap();
                });
        });
        Self {
            url: ready_rx.recv().unwrap(),
            calls,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Drop for ProbeCounter {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

struct TestDaemon(Child);
impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(paths: &HermeticPaths, mode: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_openusage-cli"));
    command
        .arg(mode)
        .arg("--plugins-dir")
        .arg(paths.root.join("plugins"))
        .arg("--app-data-dir")
        .arg(&paths.app_data)
        .arg("--plugin-overrides-dir")
        .arg(&paths.overrides);
    command
}

fn assert_snapshot(snapshot: &Value) {
    let mut stable = snapshot.clone();
    assert!(
        time::OffsetDateTime::parse(
            stable["fetchedAt"].as_str().unwrap(),
            &time::format_description::well_known::Rfc3339
        )
        .is_ok()
    );
    stable.as_object_mut().unwrap().remove("fetchedAt");
    assert_eq!(
        stable,
        json!({"providerId":"output-contract", "displayName":"Output Contract",
        "plan":"Test plan", "warning":"Estimated usage", "lines":expected_lines(),
        "account":{"id":"default", "origin":"native", "isActive":true}})
    );
}

#[test]
fn output_contract_warning_and_metrics_survive_http_cache_and_real_cli() {
    let Some(paths) =
        hermetic::enter("output_contract_warning_and_metrics_survive_http_cache_and_real_cli")
    else {
        return;
    };
    let dir = paths.root.join("plugins/output-contract");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("plugin.json"), json!({"schemaVersion":1, "id":"output-contract", "name":"Output Contract", "version":"0.0.0", "entry":"plugin.js", "icon":"icon.svg", "lines":[]}).to_string()).unwrap();
    fs::write(
        dir.join("icon.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    )
    .unwrap();
    // The synthetic plugin only calls a controlled loopback probe counter.
    let counter = ProbeCounter::start();
    fs::write(
        dir.join("plugin.js"),
        format!("globalThis.__openusage_plugin={{probe(ctx){{ctx.host.http.request({{url:{},method:'GET'}});{METRICS}}}}};", serde_json::to_string(&counter.url).unwrap()),
    )
    .unwrap();
    let mut daemon = TestDaemon(
        command(&paths, "run-daemon")
            .args([
                "--foreground=true",
                "--host=127.0.0.1",
                "--port=0",
                "--refresh-interval-secs=0",
                "--aggressive-refresh-interval-secs=3600",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let endpoint_file = config::daemon_endpoint_path().unwrap().endpoint_file;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let endpoint = loop {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited during startup"
        );
        if let Ok(endpoint) = fs::read_to_string(&endpoint_file) {
            let endpoint = endpoint.trim().to_string();
            if client
                .get(format!("{endpoint}/health"))
                .send()
                .is_ok_and(|r| r.status().is_success())
            {
                break endpoint;
            }
        }
        assert!(Instant::now() < deadline, "daemon startup timeout");
        std::thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(counter.count(), 1, "startup probes once");
    let refreshed: Value = client
        .get(format!("{endpoint}/v1/usage?refresh=true"))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(refreshed.as_array().unwrap().len(), 1);
    assert_snapshot(&refreshed[0]);
    assert_eq!(counter.count(), 2, "explicit HTTP refresh probes again");
    for route in ["/v1/usage", "/v1/usage/output-contract"] {
        let cached: Value = client
            .get(format!("{endpoint}{route}"))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(
            cached,
            if route == "/v1/usage" {
                refreshed.clone()
            } else {
                refreshed[0].clone()
            }
        );
    }
    assert_eq!(counter.count(), 2, "HTTP cache reads do not probe");
    let mut expected_probes = 2;
    for direct in [false, true] {
        for wrapped in [false, true] {
            let output = command(&paths, "query")
                .arg(format!("--use-daemon={}", !direct))
                .arg(format!("--with-state={wrapped}"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: Value = serde_json::from_slice(&output.stdout).expect("CLI JSON");
            let data = if wrapped {
                assert_eq!(
                    value["state"],
                    json!({"queryMode":if direct {"direct"} else {"cache"}})
                );
                assert_eq!(value.as_object().unwrap().len(), 2);
                &value["data"]
            } else {
                &value
            };
            assert_eq!(data.as_array().unwrap().len(), 1);
            assert_snapshot(&data[0]);
            if !direct {
                assert_eq!(data, &refreshed);
            } else {
                expected_probes += 1;
            }
            assert_eq!(
                counter.count(),
                expected_probes,
                "only direct CLI queries probe"
            );
        }
    }
    client
        .post(format!("{endpoint}/v1/shutdown"))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap();
}
