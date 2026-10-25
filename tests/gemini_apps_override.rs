#[path = "support/hermetic.rs"]
mod hermetic;

use hermetic::HermeticPaths;
use openusage_cli::plugin_engine::{manifest, runtime};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::{fs, path::Path};

const AUTH_MISSING: &str = "Gemini Apps auth missing. Log into gemini.google.com in Chrome.";
const HARNESS: &str = r#"
;(function () {
    var ctx = globalThis.__openusage_ctx;
    var cases = __CASES__;
    var path = __PATH__;
    var write = ctx.host.fs.writeText;
    var observed = {requests: [], reads: [], env: [], warnings: [], probes: [], fixtures: []};
    var initialGlobals = Object.getOwnPropertyNames(globalThis);
    var index = -1, current;
    function persist() { write(path, JSON.stringify(observed)); }
    function snapshot(object) {
        var keys = Object.getOwnPropertyNames(object);
        var values = keys.map(function (key) { return Object.getOwnPropertyDescriptor(object, key); });
        var prototype = Object.getPrototypeOf(object);
        return function () {
            if (Object.getPrototypeOf(object) !== prototype || JSON.stringify(Object.getOwnPropertyNames(object)) !== JSON.stringify(keys)) return false;
            return keys.every(function (key, i) {
                var before = values[i], after = Object.getOwnPropertyDescriptor(object, key);
                return ['value', 'get', 'set', 'writable', 'enumerable', 'configurable'].every(function (field) {
                    return Object.is(before[field], after[field]);
                });
            });
        };
    }
    var reader = {
        read: function (opts) {
            observed.reads.push({probe: index, method: 'read', opts: opts}); persist();
            if (current.readError) throw new Error('synthetic cookie read failure');
            return current.selected || {};
        },
        readProfiles: function (opts) {
            if (opts.hosts[0] === 'fixture.invalid') return current.profiles || [];
            observed.reads.push({probe: index, method: 'readProfiles', opts: opts}); persist();
            if (current.profilesError) throw new Error('synthetic cookie read failure');
            return current.profiles || [];
        },
        independent: function () { return 'independent result'; }
    };
    ctx.host.chromiumCookies = reader;
    var read = reader.read, readProfiles = reader.readProfiles, independent = reader.independent;
    ctx.host.env.get = function (name) {
        observed.env.push({probe: index, name: name}); persist();
        if (current.envError) throw new Error('synthetic env read failure');
        return name === 'GEMINI_COOKIE' ? current.env || null : null;
    };
    ctx.host.log.warn = function (message) { observed.warnings.push(message); persist(); };
    ctx.host.log.info = function () {};
    ctx.host.http.request = function (request) {
        observed.requests.push({probe: index, request: request}); persist();
        if (current.httpError) throw new Error('synthetic connection failure');
        if (current.status) return {status: current.status, bodyText: ''};
        if (request.method === 'GET') return {status: 200, bodyText: current.noMeta ? '' :
            '{"SNlM0e":"synthetic-at","cfb2h":"synthetic-build","FdrFJe":"99"}'};
        var rpc = request.url.indexOf('rpcids=jSf9Qc') >= 0 ? 'jSf9Qc' : 'sJBwce';
        var payload = rpc === 'jSf9Qc' ? (current.badUsage ? [] :
            [2, [[35531, 0.2656, 2, [[1788254602]]], [2093, 0.13, 1, [[1788200602]]]]]) : ['Pro'];
        return {status: 200, bodyText: JSON.stringify([['wrb.fr', rpc, JSON.stringify(payload)]])};
    };
    var original = globalThis.__openusage_plugin.probe;
    globalThis.__openusage_plugin.discoverAccounts = function () {
        return cases.map(function (_, i) { return {id: 'fixture-' + i}; });
    };
    globalThis.__openusage_plugin.probe = function (scoped) {
        index += 1;
        current = cases[index];
        reader.read = read; reader.readProfiles = readProfiles;
        observed.fixtures.push(reader.readProfiles({hosts: ['fixture.invalid'], names: ['fixture']}));
        if (current.noProfiles) delete reader.readProfiles;
        if (current.noRead) delete reader.read;
        if (current.noReader) delete ctx.host.chromiumCookies;
        else ctx.host.chromiumCookies = reader;
        var snapshots = [scoped, ctx, scoped.host, reader, scoped.host.env, scoped.host.http,
            scoped.host.crypto, scoped.host.fs, scoped.host.log, globalThis].map(snapshot);
        var beforeCase = JSON.stringify(current);
        try { return original(scoped); }
        finally {
            observed.probes.push({
                nowIso: scoped.nowIso,
                unchanged: snapshots.every(function (unchanged) { return unchanged(); }) &&
                    reader.read === (current.noRead ? undefined : read) &&
                    reader.readProfiles === (current.noProfiles ? undefined : readProfiles) &&
                    reader.independent === independent && reader.independent() === 'independent result' &&
                    beforeCase === JSON.stringify(current),
                privateGlobals: Object.getOwnPropertyNames(globalThis).filter(function (name) {
                    return initialGlobals.indexOf(name) < 0 && name !== '__openusage_override' &&
                        name !== 'patchReadChromeCookies' && name !== '__openusage_ast_patch';
                })
            });
            persist();
        }
    };
    persist();
})();
"#;

fn valid(marker: &str) -> Value {
    json!({"SAPISID":format!("{marker}-sap"), "__Secure-1PSID":format!("{marker}-psid")})
}

fn partial() -> Value {
    json!({"SID":"ignored", "HSID":"ignored", "SSID":"ignored", "APISID":"ignored",
        "SAPISID":"ignored", "__Secure-1PSIDTS":"ignored", "__Secure-3PSID":"ignored", "__Secure-1PAPISID":"ignored"})
}

fn probe(paths: &HermeticPaths, cases: Vec<Value>, corrected: bool) -> (Vec<Value>, Value) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut plugin = manifest::load_plugins_from_dir(&root.join("vendor/crossusage/plugins"))
        .into_iter()
        .find(|plugin| plugin.manifest.id == "gemini-apps")
        .expect("load actual pinned Gemini Apps plugin");
    if corrected {
        fs::copy(
            root.join("plugin-overrides/gemini-apps.js"),
            paths.overrides.join("gemini-apps.js"),
        )
        .expect("install maintained profile correction");
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
    assert_eq!(
        observed["probes"].as_array().unwrap().len(),
        cases.len(),
        "{result:?}"
    );
    for (i, case) in cases.iter().enumerate() {
        assert_eq!(
            observed["fixtures"][i],
            case.get("profiles").cloned().unwrap_or(json!([])),
            "readProfiles fixture works even without correction"
        );
        assert_eq!(observed["probes"][i]["unchanged"], true);
        assert_eq!(observed["probes"][i]["privateGlobals"], json!([]));
    }
    assert!(
        result.subscriptions.is_empty(),
        "native plugin declares no file watches"
    );
    let outputs = result
        .outputs
        .iter()
        .map(|output| serde_json::to_value(output).unwrap())
        .collect();
    (outputs, observed)
}

fn assert_success(output: &Value, source: &str) {
    assert_eq!(output["plan"], "Pro", "{output}");
    assert_eq!(output["lines"].as_array().unwrap().len(), 3);
    for (i, label, used, reset, duration) in [
        (0, "Current", 13.0, "2026-08-31T18:23:22.000Z", 18_000_000),
        (1, "Weekly", 27.0, "2026-09-01T09:23:22.000Z", 604_800_000),
    ] {
        let line = &output["lines"][i];
        assert_eq!(line["type"], "progress");
        assert_eq!(line["label"], label);
        assert_eq!(line["used"], used);
        assert_eq!(line["limit"], 100.0);
        assert_eq!(line["format"], json!({"kind":"percent"}));
        assert_eq!(line["resetsAt"], reset);
        assert_eq!(line["periodDurationMs"], duration);
    }
    assert_eq!(output["lines"][2]["type"], "text");
    assert_eq!(output["lines"][2]["label"], "Source");
    assert_eq!(output["lines"][2]["value"], source);
}

fn assert_error(output: &Value, message: &str) {
    assert_eq!(output["lines"][0]["label"], "Error", "{output}");
    assert_eq!(output["lines"][0]["text"], message, "{output}");
}

fn requests(observed: &Value, index: usize) -> Vec<&Value> {
    observed["requests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["probe"] == index)
        .map(|entry| &entry["request"])
        .collect()
}

fn assert_requests(observed: &Value, index: usize, cookie: &str) {
    let requests = requests(observed, index);
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request["headers"]["Cookie"], cookie);
        assert_eq!(request["headers"]["Origin"], "https://gemini.google.com");
        assert_eq!(
            request["headers"]["Referer"],
            "https://gemini.google.com/usage"
        );
        assert_eq!(request["headers"]["X-Same-Domain"], "1");
        assert!(
            request["headers"]["User-Agent"]
                .as_str()
                .unwrap()
                .contains("Chrome/144.0.0.0")
        );
        assert_eq!(request["http1Only"], true);
        assert_eq!(request["timeoutMs"], 15000);
        let timestamp = chrono::DateTime::parse_from_rfc3339(
            observed["probes"][index]["nowIso"].as_str().unwrap(),
        )
        .unwrap()
        .timestamp();
        let sapisid = cookie
            .split("; ")
            .find_map(|part| {
                part.strip_prefix("SAPISID=")
                    .or_else(|| part.strip_prefix("__Secure-1PAPISID="))
            })
            .unwrap();
        let digest =
            Sha1::digest(format!("{timestamp} {sapisid} https://gemini.google.com").as_bytes());
        assert_eq!(
            request["headers"]["Authorization"],
            format!("SAPISIDHASH {timestamp}_{digest:x}")
        );
    }
    assert_eq!(requests[0]["method"], "GET");
    assert_eq!(requests[0]["url"], "https://gemini.google.com/app");
    for request in &requests[1..] {
        assert_eq!(request["method"], "POST");
        assert!(
            request["bodyText"]
                .as_str()
                .unwrap()
                .contains("at=synthetic-at")
        );
    }
}

#[test]
fn gemini_apps_native_eight_cookie_profile_masks_valid_two_cookie_profile() {
    let Some(paths) =
        hermetic::enter("gemini_apps_native_eight_cookie_profile_masks_valid_two_cookie_profile")
    else {
        return;
    };
    let cases = vec![json!({"profiles":[partial(), valid("later")], "selected":partial()})];
    let (native, native_observed) = probe(&paths, cases.clone(), false);
    assert_error(&native[0], AUTH_MISSING);
    assert!(native_observed["requests"].as_array().unwrap().is_empty());
    let (outputs, observed) = probe(&paths, cases, true);
    assert_success(&outputs[0], "Chrome");
    assert_requests(&observed, 0, "SAPISID=later-sap; __Secure-1PSID=later-psid");
    assert_eq!(observed["env"], json!([]));
    assert_eq!(
        observed["reads"],
        json!([{"probe":0,"method":"readProfiles","opts":{
        "hosts":[".google.com","google.com",".gemini.google.com","gemini.google.com"],
        "names":["SID","HSID","SSID","APISID","SAPISID","__Secure-1PSID","__Secure-1PSIDTS","__Secure-3PSID","__Secure-1PAPISID"]}}])
    );
}

#[test]
fn gemini_apps_first_valid_native_source_precedes_env_and_probes_are_isolated() {
    let Some(paths) = hermetic::enter(
        "gemini_apps_first_valid_native_source_precedes_env_and_probes_are_isolated",
    ) else {
        return;
    };
    let cases = vec![
        json!({"profiles":[partial(), valid("first"), valid("ignored")], "env":"SAPISID=env; __Secure-1PSID=env"}),
        json!({"profiles":[{"__Secure-1PAPISID":"alternate", "__Secure-1PSID":"fresh"}], "env":"SID=ignored"}),
        json!({"profiles":[{"__Secure-1PSID":"alone"},{"SAPISID":"alone"}]}),
        json!({"profiles":[valid("after-failure")]}),
    ];
    let (outputs, observed) = probe(&paths, cases, true);
    for i in [0, 1, 3] {
        assert_success(&outputs[i], "Chrome");
    }
    assert_requests(&observed, 0, "SAPISID=first-sap; __Secure-1PSID=first-psid");
    assert_requests(
        &observed,
        1,
        "__Secure-1PSID=fresh; __Secure-1PAPISID=alternate",
    );
    assert_error(&outputs[2], AUTH_MISSING);
    assert!(
        requests(&observed, 2).is_empty(),
        "partial profiles must not be stitched"
    );
    assert_requests(
        &observed,
        3,
        "SAPISID=after-failure-sap; __Secure-1PSID=after-failure-psid",
    );
    assert_eq!(observed["env"], json!([{"probe":2,"name":"GEMINI_COOKIE"}]));
}

#[test]
fn gemini_apps_native_env_errors_and_unsupported_controls_are_preserved() {
    let Some(paths) =
        hermetic::enter("gemini_apps_native_env_errors_and_unsupported_controls_are_preserved")
    else {
        return;
    };
    let env = "Cookie: SAPISID=env-sap; __Secure-1PSID=env-psid";
    let cases = vec![
        json!({"profiles":[], "env":env}),
        json!({"profiles":[partial()], "selected":partial(), "env":env}),
        json!({"profilesError":true,"readError":true,"env":env}),
        json!({"profilesError":true,"readError":true}),
        json!({"noProfiles":true,"selected":valid("legacy")}),
        json!({"noReader":true,"env":env}),
        json!({"noRead":true,"profiles":[valid("ignored")],"env":env}),
        json!({"profiles":[],"env":"SID=not-a-session"}),
        json!({"envError":true}),
    ];
    let (native, native_observed) = probe(&paths, cases.clone(), false);
    let (outputs, observed) = probe(&paths, cases, true);
    assert_eq!(outputs, native);
    assert_eq!(observed["warnings"], native_observed["warnings"]);
    for i in [0, 1, 2, 5, 6] {
        assert_success(&outputs[i], "GEMINI_COOKIE");
        assert_requests(&observed, i, "SAPISID=env-sap; __Secure-1PSID=env-psid");
    }
    assert_success(&outputs[4], "Chrome");
    assert_requests(
        &observed,
        4,
        "SAPISID=legacy-sap; __Secure-1PSID=legacy-psid",
    );
    for i in [3, 7, 8] {
        assert_error(&outputs[i], AUTH_MISSING);
        assert!(requests(&observed, i).is_empty());
    }
    assert_eq!(
        observed["warnings"],
        json!([
            "chromium cookie read failed: Error: synthetic cookie read failure",
            "chromium cookie read failed: Error: synthetic cookie read failure",
            "env read failed for GEMINI_COOKIE: Error: synthetic env read failure"
        ])
    );
    assert_eq!(
        observed["reads"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "read")
            .count(),
        1,
        "readProfiles failures must not retry another reader destination"
    );
}

#[test]
fn gemini_apps_native_http_failures_are_unchanged() {
    let Some(paths) = hermetic::enter("gemini_apps_native_http_failures_are_unchanged") else {
        return;
    };
    let mut cases = vec![
        json!({"status":302}),
        json!({"status":401}),
        json!({"status":503}),
        json!({"httpError":true}),
        json!({"noMeta":true}),
        json!({"badUsage":true}),
    ];
    for case in &mut cases {
        case["profiles"] = json!([valid("same")]);
        case["selected"] = valid("same");
    }
    let (native, _) = probe(&paths, cases.clone(), false);
    let (outputs, _) = probe(&paths, cases, true);
    assert_eq!(outputs, native);
    for (i, message) in [
        "Gemini Apps session expired. Log into gemini.google.com in Chrome.",
        "Gemini Apps session expired. Log into gemini.google.com in Chrome.",
        "Gemini Apps page failed (HTTP 503).",
        "Could not reach gemini.google.com. Check your connection.",
        "Could not read Gemini Apps session token. Log into gemini.google.com in Chrome.",
        "Could not parse Gemini Apps usage.",
    ]
    .iter()
    .enumerate()
    {
        assert_error(&outputs[i], message);
    }
}

#[test]
fn gemini_apps_single_valid_profile_preserves_native_success() {
    let Some(paths) = hermetic::enter("gemini_apps_single_valid_profile_preserves_native_success")
    else {
        return;
    };
    let cases = vec![json!({"profiles":[valid("same")], "selected":valid("same"),
        "env":"SAPISID=ignored; __Secure-1PSID=ignored"})];
    let (native, native_observed) = probe(&paths, cases.clone(), false);
    let (outputs, observed) = probe(&paths, cases, true);
    assert_eq!(outputs, native);
    assert_success(&outputs[0], "Chrome");
    for interaction in [&observed, &native_observed] {
        assert_requests(interaction, 0, "SAPISID=same-sap; __Secure-1PSID=same-psid");
        assert_eq!(interaction["env"], json!([]));
        assert_eq!(interaction["warnings"], json!([]));
    }
}
