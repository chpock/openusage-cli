#[path = "support/hermetic.rs"]
mod hermetic;

use hermetic::HermeticPaths;
use openusage_cli::plugin_engine::{manifest, runtime};
use serde_json::{Value, json};
use std::{fs, path::Path};

const HARNESS: &str = r#"
;(function () {
    var ctx = globalThis.__openusage_ctx;
    var cases = __CASES__;
    var path = __PATH__;
    var writeFile = ctx.host.fs.writeText;
    var observed = {probes: [], writes: [], reads: [], requests: [], warnings: [], infos: []};
    var initialGlobals = Object.getOwnPropertyNames(globalThis);
    var current, selected, index = -1;
    var now = 1800000000000;
    Date.now = function () { return now; };
    function persist() { writeFile(path, JSON.stringify(observed)); }
    function recordWrite(selector, service, account, value) {
        var entry = {probe: index, selector: selector, service: service, account: account, value: value};
        observed.writes.push(entry);
        persist();
        if (current.removeSource) current.entries = current.entries.filter(function (e) { return e.id !== 'source'; });
        if (current.writeFails) throw new Error('synthetic write failure');
        var target = selected[selector + ':' + service + ':' + account];
        if (target && current.entries.indexOf(target) < 0) throw new Error('selected source removed');
        if (!target) {
            target = current.entries.find(function (e) {
                return e.service === service && (selector === 'generic' || e.username === account);
            });
        }
        if (!target) {
            target = {id: 'created', service: service, username: account};
            current.entries.push(target);
        }
        target.raw = value;
        entry.destination = target.id;
        persist();
    }
    // All writers are instrumented before the override or native probe can call them.
    ctx.host.keychain = {
        readGenericPassword: function (service, account) {
            observed.reads.push({probe: index, service: service, account: account === undefined ? null : account});
            persist();
            if (account === 'antigravity' && current.explicitReadFails) throw new Error('synthetic explicit read failure');
            var entry = current.entries.find(function (e) {
                return e.service === service && (account === undefined || e.username === account);
            });
            if (!entry) return null;
            if (entry.raw) selected[(account === undefined ? 'generic' : 'explicit') + ':' + service + ':' + (account === undefined ? null : account)] = entry;
            return entry.raw;
        },
        writeGenericPassword: function (service, value) { return recordWrite('generic', service, null, value); },
        writeGenericPasswordForAccount: function (service, account, value) { return recordWrite('explicit', service, account, value); },
        writeGenericPasswordForCurrentUser: function () { observed.writes.push({unexpected: 'current-user'}); persist(); throw new Error('unexpected current-user write'); }
    };
    var keychain = ctx.host.keychain;
    var read = keychain.readGenericPassword;
    var genericWrite = keychain.writeGenericPassword;
    var explicitWrite = keychain.writeGenericPasswordForAccount;
    ctx.host.log.warn = function (message) { observed.warnings.push(message); persist(); };
    ctx.host.log.info = function (message) { observed.infos.push(message); persist(); };
    ctx.host.http.request = function (request) {
        observed.requests.push({probe: index, request: request});
        persist();
        if (request.url === 'https://oauth2.googleapis.com/token') {
            if (current.oauthThrows) throw new Error('synthetic network failure');
            return {status: current.oauthStatus || 200, bodyText: JSON.stringify(current.oauthBody || {access_token: 'fresh-' + index, expires_in: 3600})};
        }
        if (current.requestThrows) throw new Error('synthetic network failure');
        var status = current.quotaStatus || 200;
        if (current.reactive && request.headers.Authorization === 'Bearer old-' + index) {
            status = 401;
            if (current.retryEntries) current.entries = current.retryEntries;
        }
        return {status: status, bodyText: JSON.stringify({groups: [{buckets: [
            {bucketId: 'gemini-5h', remainingFraction: 0.75, resetTime: '2027-01-16T09:00:00Z'}
        ]}]})};
    };
    var original = globalThis.__openusage_plugin.probe;
    globalThis.__openusage_plugin.discoverAccounts = function () {
        return cases.map(function (_, i) { return {id: 'fixture-' + i}; });
    };
    globalThis.__openusage_plugin.probe = function (scoped) {
        index += 1;
        current = cases[index];
        selected = {};
        try {
            if (current.preflight) scoped.host.keychain.writeGenericPasswordForAccount('gemini', 'antigravity', current.preflight);
            var result = original(scoped);
            if (current.independentWrites) {
                scoped.host.keychain.readGenericPassword('unrelated-service', 'unrelated-user');
                scoped.host.keychain.writeGenericPasswordForAccount('gemini', 'unrelated-user', 'independent account bytes');
                scoped.host.keychain.writeGenericPasswordForAccount('unrelated-service', 'antigravity', 'independent service bytes');
                scoped.host.keychain.writeGenericPassword('unrelated-generic', 'independent generic bytes');
                // An explicit write on the shared host is independent, even after fallback refresh.
                ctx.host.keychain.writeGenericPasswordForAccount('gemini', 'antigravity', 'independent fixed bytes');
            }
            return result;
        } finally {
            var restored = ctx.host.keychain === keychain && keychain.readGenericPassword === read &&
                keychain.writeGenericPassword === genericWrite && keychain.writeGenericPasswordForAccount === explicitWrite;
            var privateGlobals = Object.getOwnPropertyNames(globalThis).filter(function (name) {
                return initialGlobals.indexOf(name) < 0 && name !== '__openusage_override';
            });
            var probe = {entries: current.entries, restored: restored, privateGlobals: privateGlobals};
            observed.probes.push(probe);
            persist();
        }
    };
    persist();
})();
"#;

fn credentials(index: usize, expired: bool) -> String {
    json!({"keep":"synthetic-byte-marker", "token":{
        "access_token":format!("old-{index}"), "refresh_token":format!("refresh-{index}"),
        "expiry": if expired {"2000-01-01T00:00:00Z"} else {"2099-01-01T00:00:00Z"},
        "custom":"preserved"
    }})
    .to_string()
}

fn case(index: usize, username: Option<&str>, expired: bool) -> Value {
    json!({"entries":[{"id":"source", "service":"gemini", "username":username,
        "raw":credentials(index, expired)}]})
}

fn probe(paths: &HermeticPaths, cases: Vec<Value>, corrected: bool) -> (Vec<Value>, Value) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut plugin = manifest::load_plugins_from_dir(&root.join("vendor/crossusage/plugins"))
        .into_iter()
        .find(|plugin| plugin.manifest.id == "antigravity-cli")
        .expect("load the actual pinned Antigravity CLI plugin");
    if corrected {
        fs::copy(
            root.join("plugin-overrides/antigravity-cli.js"),
            paths.overrides.join("antigravity-cli.js"),
        )
        .expect("install the maintained caller correction");
    }
    let observation = paths.root.join("observed.json");
    plugin.entry_script.push_str(
        &HARNESS
            .replace("__CASES__", &serde_json::to_string(&cases).unwrap())
            .replace(
                "__PATH__",
                &serde_json::to_string(&observation.to_string_lossy()).unwrap(),
            ),
    );
    let result = runtime::run_probe_in_sandbox(
        &plugin,
        &paths.app_data,
        "test",
        corrected.then_some(paths.overrides.as_path()),
        &paths.root,
    );
    let observed: Value = serde_json::from_str(&fs::read_to_string(observation).unwrap()).unwrap();
    assert_eq!(observed["probes"].as_array().unwrap().len(), cases.len());
    for probe in observed["probes"].as_array().unwrap() {
        assert_eq!(
            probe["restored"], true,
            "shared keychain API must be untouched"
        );
        assert_eq!(
            probe["privateGlobals"],
            json!([]),
            "no private registry or factory may be published"
        );
    }
    let outputs = result
        .outputs
        .iter()
        .map(|output| serde_json::to_value(output).unwrap())
        .collect();
    (outputs, observed)
}

fn assert_success(output: &Value) {
    assert_eq!(output["lines"].as_array().unwrap().len(), 1, "{output}");
    let line = &output["lines"][0];
    assert_eq!(line["type"], "progress", "{output}");
    assert_eq!(line["label"], "Session");
    assert_eq!(line["used"], 25.0);
    assert_eq!(line["limit"], 100.0);
    assert_eq!(line["format"], json!({"kind":"percent"}));
    assert_eq!(line["periodDurationMs"], 18_000_000);
    assert_eq!(line["resetsAt"], "2027-01-16T09:00:00.000Z");
}

fn assert_error(output: &Value, message: &str) {
    assert_eq!(output["lines"][0]["label"], "Error", "{output}");
    assert_eq!(output["lines"][0]["text"], message, "{output}");
}

fn assert_refresh(observed: &Value, index: usize, selector: &str, written: bool, reactive: bool) {
    let writes: Vec<&Value> = observed["writes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["probe"] == index)
        .collect();
    assert_eq!(writes.len(), 1, "all lifecycle writes: {observed}");
    let write = writes[0];
    assert_eq!(write["selector"], selector);
    assert_eq!(write["service"], "gemini");
    assert_eq!(
        write["account"],
        if selector == "explicit" {
            json!("antigravity")
        } else {
            Value::Null
        }
    );
    let mut expected: Value = serde_json::from_str(&credentials(index, !reactive)).unwrap();
    expected["token"]["access_token"] = json!(format!("fresh-{index}"));
    expected["token"]["token_type"] = json!("Bearer");
    expected["token"]["expiry"] = json!("2027-01-15T09:00:00.000Z");
    assert_eq!(
        serde_json::from_str::<Value>(write["value"].as_str().unwrap()).unwrap(),
        expected
    );
    assert_eq!(
        write["destination"],
        if written {
            json!("source")
        } else {
            Value::Null
        }
    );
    let requests: Vec<&Value> = observed["requests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["probe"] == index)
        .map(|r| &r["request"])
        .collect();
    assert_eq!(requests.len(), if reactive { 3 } else { 2 });
    let oauth = requests[usize::from(reactive)];
    assert_eq!(
        oauth,
        &json!({"method":"POST", "url":"https://oauth2.googleapis.com/token",
        "headers":{"Content-Type":"application/x-www-form-urlencoded"},
        "bodyText":format!("client_id=1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com&client_secret=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&refresh_token=refresh-{index}&grant_type=refresh_token"),
        "timeoutMs":15000})
    );
    let quota = |token: String| {
        json!({"method":"POST", "url":"https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary",
        "headers":{"Authorization":format!("Bearer {token}"), "Accept":"application/json", "Content-Type":"application/json", "User-Agent":"antigravity"},
        "bodyText":"{}", "timeoutMs":15000})
    };
    if reactive {
        assert_eq!(requests[0], &quota(format!("old-{index}")));
    }
    assert_eq!(
        requests[requests.len() - 1],
        &quota(format!("fresh-{index}"))
    );
    let reads: Vec<Value> = observed["reads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["probe"] == index)
        .map(|r| r["account"].clone())
        .collect();
    let once = if selector == "explicit" {
        vec![json!("antigravity")]
    } else {
        vec![json!("antigravity"), Value::Null]
    };
    let expected_reads: Vec<Value> = once
        .iter()
        .cycle()
        .take(once.len() * if reactive { 2 } else { 1 })
        .cloned()
        .collect();
    assert_eq!(reads, expected_reads);
}

#[test]
fn antigravity_cli_refresh_preserves_successful_read_selector() {
    let Some(paths) = hermetic::enter("antigravity_cli_refresh_preserves_successful_read_selector")
    else {
        return;
    };
    let cases: Vec<Value> = [Some("antigravity"), Some("another-user"), None]
        .into_iter()
        .cycle()
        .take(6)
        .enumerate()
        .map(|(i, username)| {
            let mut value = case(i, username, i < 3);
            value["reactive"] = json!(i >= 3);
            value
        })
        .collect();
    let (outputs, observed) = probe(&paths, cases, true);
    assert_eq!(outputs.len(), 6);
    for (i, output) in outputs.iter().enumerate() {
        assert_success(output);
        assert_refresh(
            &observed,
            i,
            if i % 3 == 0 { "explicit" } else { "generic" },
            true,
            i >= 3,
        );
        assert_eq!(
            observed["probes"][i]["entries"].as_array().unwrap().len(),
            1,
            "must not create the fixed account"
        );
        assert_eq!(
            observed["probes"][i]["entries"][0]["raw"],
            observed["writes"][i]["value"]
        );
    }
    assert_eq!(observed["warnings"], json!([]));
}

#[test]
fn antigravity_cli_fallback_does_not_overwrite_existing_explicit_account() {
    let Some(paths) =
        hermetic::enter("antigravity_cli_fallback_does_not_overwrite_existing_explicit_account")
    else {
        return;
    };
    let mut value = case(0, Some("another-user"), true);
    value["explicitReadFails"] = json!(true);
    let sentinel = json!({"id":"fixed", "service":"gemini", "username":"antigravity", "raw":"unchanged synthetic bytes\n"});
    value["entries"]
        .as_array_mut()
        .unwrap()
        .push(sentinel.clone());
    let (outputs, observed) = probe(&paths, vec![value], true);
    assert_success(&outputs[0]);
    assert_refresh(&observed, 0, "generic", true, false);
    assert_eq!(observed["probes"][0]["entries"][1], sentinel);
    assert_eq!(
        observed["probes"][0]["entries"].as_array().unwrap().len(),
        2
    );
    assert!(
        observed["infos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap().contains("account keychain read failed"))
    );
}

#[test]
fn antigravity_cli_selected_write_failure_never_retargets() {
    let Some(paths) = hermetic::enter("antigravity_cli_selected_write_failure_never_retargets")
    else {
        return;
    };
    let mut cases = Vec::new();
    for i in 0..6 {
        let mut value = case(
            i,
            if i >= 4 {
                Some("antigravity")
            } else if i % 2 == 0 {
                Some("another-user")
            } else {
                None
            },
            true,
        );
        value[if i < 2 || i == 4 {
            "writeFails"
        } else {
            "removeSource"
        }] = json!(true);
        value["entries"].as_array_mut().unwrap().push(json!({"id":"other", "service":"gemini", "username":"antigravity", "raw":"unchanged synthetic bytes"}));
        value["explicitReadFails"] = json!(i < 4);
        cases.push(value);
    }
    let (outputs, observed) = probe(&paths, cases, true);
    for (i, output) in outputs.iter().enumerate() {
        assert_success(output);
        assert_refresh(
            &observed,
            i,
            if i < 4 { "generic" } else { "explicit" },
            false,
            false,
        );
        let entries = observed["probes"][i]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), if i < 2 || i == 4 { 2 } else { 1 });
        assert_eq!(entries.last().unwrap()["raw"], "unchanged synthetic bytes");
        if i < 2 || i == 4 {
            assert_eq!(entries[0]["raw"], credentials(i, true));
        }
    }
    assert_eq!(observed["writes"].as_array().unwrap().len(), 6);
    assert_eq!(observed["warnings"].as_array().unwrap().len(), 6);
    for warning in observed["warnings"].as_array().unwrap() {
        assert!(
            warning
                .as_str()
                .unwrap()
                .starts_with("antigravity-cli keychain write failed:")
        );
    }
}

#[test]
fn antigravity_cli_non_refresh_and_native_errors_remain_unchanged() {
    let Some(paths) =
        hermetic::enter("antigravity_cli_non_refresh_and_native_errors_remain_unchanged")
    else {
        return;
    };
    let mut cases = vec![
        case(0, Some("another-user"), false),
        json!({"entries":[]}),
        case(2, None, true),
        case(3, Some("antigravity"), false),
        case(4, None, false),
        case(5, Some("antigravity"), false),
        case(6, None, true),
        case(7, Some("another-user"), true),
        case(8, None, false),
    ];
    cases[2]["oauthStatus"] = json!(400);
    cases[3]["requestThrows"] = json!(true);
    cases[4]["quotaStatus"] = json!(401);
    cases[4]["oauthStatus"] = json!(400);
    cases[5]["entries"][0]["raw"] = json!("Bearer synthetic-raw-token");
    cases[6]["oauthThrows"] = json!(true);
    cases[7]["oauthBody"] = json!({"unexpected":"missing-access-token"});
    cases[8]["entries"][0]["raw"] = json!("{\"token\":{}}");
    let (native_outputs, native_observed) = probe(&paths, cases.clone(), false);
    let (outputs, observed) = probe(&paths, cases.clone(), true);
    assert_eq!(outputs, native_outputs);
    assert_eq!(observed, native_observed);
    assert_success(&outputs[0]);
    assert_error(
        &outputs[1],
        "Not logged in. Run `agy` and complete Google sign-in first.",
    );
    assert_error(
        &outputs[2],
        "Google sign-in expired. Run `agy` and complete Google sign-in again.",
    );
    assert_error(
        &outputs[3],
        "Antigravity CLI quota request failed. Check your connection and try again.",
    );
    assert_error(
        &outputs[4],
        "Google sign-in expired. Run `agy` and complete Google sign-in again.",
    );
    assert_success(&outputs[5]);
    for i in [6, 7] {
        assert_error(
            &outputs[i],
            "Google sign-in expired. Run `agy` and complete Google sign-in again.",
        );
    }
    assert_error(
        &outputs[8],
        "Not logged in. Run `agy` and complete Google sign-in first.",
    );
    assert_eq!(observed["writes"], json!([]));
    for (i, case) in cases.iter().enumerate() {
        assert_eq!(observed["probes"][i]["entries"], case["entries"]);
    }
}

#[test]
fn antigravity_cli_failed_and_sequential_probes_do_not_reuse_selector() {
    let Some(paths) =
        hermetic::enter("antigravity_cli_failed_and_sequential_probes_do_not_reuse_selector")
    else {
        return;
    };
    let mut cases = vec![
        case(0, None, true),
        case(1, Some("antigravity"), false),
        case(2, Some("another-user"), true),
        case(3, Some("antigravity"), false),
        json!({"entries":[]}),
    ];
    cases[0]["oauthStatus"] = json!(400);
    cases[1]["preflight"] = json!(credentials(1, false));
    cases[3]["preflight"] = json!(credentials(3, false));
    let (outputs, observed) = probe(&paths, cases, true);
    assert_error(
        &outputs[0],
        "Google sign-in expired. Run `agy` and complete Google sign-in again.",
    );
    assert_success(&outputs[1]);
    assert_success(&outputs[2]);
    assert_success(&outputs[3]);
    assert_error(
        &outputs[4],
        "Not logged in. Run `agy` and complete Google sign-in first.",
    );
    assert_refresh(&observed, 2, "generic", true, false);
    let writes = observed["writes"].as_array().unwrap();
    assert_eq!(writes.len(), 3);
    for (write, index) in [(0, 1), (2, 3)] {
        assert_eq!(
            writes[write],
            json!({"probe":index, "selector":"explicit", "service":"gemini", "account":"antigravity", "value":credentials(index, false), "destination":"source"})
        );
    }
    assert_eq!(
        observed["probes"][0]["entries"][0]["raw"],
        credentials(0, true)
    );
    assert_eq!(observed["probes"][4]["entries"], json!([]));
}

#[test]
fn antigravity_cli_wrapper_does_not_retarget_independent_selectors_or_shared_api() {
    let Some(paths) = hermetic::enter(
        "antigravity_cli_wrapper_does_not_retarget_independent_selectors_or_shared_api",
    ) else {
        return;
    };
    let mut value = case(0, Some("another-user"), true);
    value["independentWrites"] = json!(true);
    let (outputs, observed) = probe(&paths, vec![value], true);
    assert_success(&outputs[0]);
    let writes = observed["writes"].as_array().unwrap();
    assert_eq!(
        writes.len(),
        5,
        "observe every write through the end of the lifecycle"
    );
    assert_eq!(writes[0]["selector"], "generic");
    assert_eq!(writes[0]["destination"], "source");
    for (i, selector, service, account, value) in [
        (
            1,
            "explicit",
            "gemini",
            json!("unrelated-user"),
            "independent account bytes",
        ),
        (
            2,
            "explicit",
            "unrelated-service",
            json!("antigravity"),
            "independent service bytes",
        ),
        (
            3,
            "generic",
            "unrelated-generic",
            Value::Null,
            "independent generic bytes",
        ),
        (
            4,
            "explicit",
            "gemini",
            json!("antigravity"),
            "independent fixed bytes",
        ),
    ] {
        assert_eq!(
            writes[i],
            json!({"probe":0, "selector":selector, "service":service, "account":account, "value":value, "destination":"created"})
        );
    }
    assert_eq!(
        observed["probes"][0]["entries"][0]["raw"],
        writes[0]["value"]
    );
}

#[test]
fn antigravity_cli_encoded_payload_refresh_keeps_native_parsing() {
    use base64::Engine;
    let Some(paths) =
        hermetic::enter("antigravity_cli_encoded_payload_refresh_keeps_native_parsing")
    else {
        return;
    };
    let mut value = case(0, None, true);
    value["entries"][0]["raw"] = json!(format!(
        "go-keyring-base64:{}",
        base64::engine::general_purpose::STANDARD.encode(credentials(0, true))
    ));
    let (outputs, observed) = probe(&paths, vec![value], true);
    assert_success(&outputs[0]);
    assert_refresh(&observed, 0, "generic", true, false);
    assert_eq!(
        observed["probes"][0]["entries"][0]["raw"],
        observed["writes"][0]["value"]
    );
}

#[test]
fn antigravity_cli_reactive_refresh_uses_retry_read_not_initial_read() {
    let Some(paths) =
        hermetic::enter("antigravity_cli_reactive_refresh_uses_retry_read_not_initial_read")
    else {
        return;
    };
    let mut cases = vec![case(0, Some("antigravity"), false), case(1, None, false)];
    cases[0]["retryEntries"] = case(0, None, false)["entries"].clone();
    cases[1]["retryEntries"] = case(1, Some("antigravity"), false)["entries"].clone();
    for value in &mut cases {
        value["reactive"] = json!(true);
    }
    let (outputs, observed) = probe(&paths, cases, true);
    let writes = observed["writes"].as_array().unwrap();
    assert_eq!(writes.len(), 2);
    for (i, output) in outputs.iter().enumerate() {
        assert_success(output);
        assert_eq!(
            writes[i]["selector"],
            if i == 0 { "generic" } else { "explicit" }
        );
        assert_eq!(writes[i]["destination"], "source");
        assert_eq!(
            observed["probes"][i]["entries"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            observed["probes"][i]["entries"][0]["raw"],
            writes[i]["value"]
        );
        let payload: Value = serde_json::from_str(writes[i]["value"].as_str().unwrap()).unwrap();
        assert_eq!(payload["token"]["access_token"], format!("fresh-{i}"));
        assert_eq!(payload["token"]["refresh_token"], format!("refresh-{i}"));
    }
    assert_eq!(
        observed["reads"],
        json!([
            {"probe":0,"service":"gemini","account":"antigravity"},
            {"probe":0,"service":"gemini","account":"antigravity"},
            {"probe":0,"service":"gemini","account":null},
            {"probe":1,"service":"gemini","account":"antigravity"},
            {"probe":1,"service":"gemini","account":null},
            {"probe":1,"service":"gemini","account":"antigravity"}
        ])
    );
    assert_eq!(observed["requests"].as_array().unwrap().len(), 6);
    assert_eq!(observed["warnings"], json!([]));
}
