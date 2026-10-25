use super::*;
use crate::hermetic;
use hermetic::HermeticPaths;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn enter(case: &str) -> Option<HermeticPaths> {
    hermetic::enter(case.strip_prefix("openusage_cli::").unwrap())
}

const CSV: &str = "usage_type,prompt_tokens,completion_tokens\ninference_usage,1200,300\ninference_usage,250,50\nother_usage,9999,9999\n";
const OPTIONS: &str = r#"{"apiKey":" selected-test-key ","accountId":" fixture-account ","startTime":" 2026-09-24 ","endTime":" 2026-10-24 "}"#;

fn fixture(paths: &HermeticPaths, behavior: &str) -> Exporter {
    let runner = paths.root.join("firectl-fixture");
    let report = paths.root.join("interactions.jsonl");
    let script = format!(
        r#"#!/usr/bin/python3
import json, os, pathlib, stat, subprocess, sys, time
root = pathlib.Path.cwd()
report = pathlib.Path({report:?})
def emit(value):
    with report.open('a') as f: f.write(json.dumps(value) + '\n')
is_help = sys.argv[1:] == ['--help']
entry = {{'argv': sys.argv[1:], 'cwd': str(root), 'environment': dict(os.environ),
    'root_mode': stat.S_IMODE(root.stat().st_mode),
    'directory_modes': {{k: stat.S_IMODE(pathlib.Path(os.environ[k]).stat().st_mode) for k in
        ['HOME','XDG_CONFIG_HOME','XDG_DATA_HOME','XDG_CACHE_HOME','XDG_STATE_HOME','XDG_RUNTIME_DIR','TMPDIR','TEMPDIR']}},
    'auth_exists': (root / '.fireworks/auth.ini').exists()}}
if not is_help:
    entry['auth'] = (root / '.fireworks/auth.ini').read_text()
    entry['auth_mode'] = stat.S_IMODE((root / '.fireworks/auth.ini').stat().st_mode)
    entry['auth_dir_mode'] = stat.S_IMODE((root / '.fireworks').stat().st_mode)
emit(entry)
behavior = {behavior:?}
if is_help:
    if behavior == 'bad_help': sys.exit(4)
    if behavior == 'help_timeout': time.sleep(60)
    if behavior == 'bad_auth': (root / '.fireworks').write_text('block auth preparation')
    sys.exit(0)
if behavior == 'fail':
    print(os.environ['FIREWORKS_API_KEY'], file=sys.stderr)
    print(os.environ['FIREWORKS_API_KEY'])
    sys.exit(3)
if behavior == 'timeout': time.sleep(60)
if behavior == 'descendant':
    child = subprocess.Popen(['/bin/sleep','60'])
    emit({{'descendant_pid': child.pid}})
if behavior == 'missing': sys.exit(0)
filename = sys.argv[sys.argv.index('--filename') + 1]
pathlib.Path(filename).write_text('  \n' if behavior == 'empty' else {csv:?})
"#,
        report = report.to_string_lossy(),
        csv = CSV,
    );
    fs::write(&runner, script).unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
    Exporter {
        candidates: vec![runner],
        environment: helper_environment(
            [
                ("PATH".into(), "/usr/bin:/bin".into()),
                (
                    "HTTPS_PROXY".into(),
                    "http://wrong-inherited.invalid:8080".into(),
                ),
                (
                    "SSL_CERT_FILE".into(),
                    paths.root.join("synthetic-ca.pem").into_os_string(),
                ),
                ("FIREWORKS_API_KEY".into(), "wrong-inherited-key".into()),
                ("FIRECTL_CONFIG".into(), "/must-not-use/config".into()),
                ("HOME".into(), "/must-not-use/home".into()),
                ("DBUS_SESSION_BUS_ADDRESS".into(), "must-not-use".into()),
            ],
            [
                ("HTTPS_PROXY".into(), "http://fixture.invalid:8080".into()),
                ("NO_PROXY".into(), "fixture.invalid".into()),
            ],
        ),
        temp_root: paths.root.join("tmp"),
        timeout: FIRECTL_TIMEOUT,
    }
}

fn interactions(paths: &HermeticPaths) -> Vec<Value> {
    fs::read_to_string(paths.root.join("interactions.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_clean(paths: &HermeticPaths) {
    assert_eq!(fs::read_dir(paths.root.join("tmp")).unwrap().count(), 0);
}

#[test]
fn rejects_invalid_options_before_any_runner_or_filesystem_work() {
    let Some(paths) = enter(concat!(
        module_path!(),
        "::rejects_invalid_options_before_any_runner_or_filesystem_work"
    )) else {
        return;
    };
    let exporter = fixture(&paths, "ok");
    for raw in [
        "",
        "null",
        "[]",
        "42",
        "{}",
        r#"{"apiKey":42}"#,
        r#"{"apiKey":"key","accountId":"account","startTime":"today"}"#,
        r#"{"apiKey":"key\n[other]","accountId":"account","startTime":"today","endTime":"tomorrow"}"#,
        r#"{"apiKey":"key","accountId":"\u0000","startTime":"today","endTime":"tomorrow"}"#,
    ] {
        assert_eq!(
            exporter.export(raw),
            json!({"status":"invalid_opts"}),
            "{raw}"
        );
    }
    assert!(!paths.root.join("interactions.jsonl").exists());
    assert_clean(&paths);
}

#[test]
fn exports_csv_with_exact_arguments_private_selected_credentials_and_cleanup() {
    let Some(paths) = enter(concat!(
        module_path!(),
        "::exports_csv_with_exact_arguments_private_selected_credentials_and_cleanup"
    )) else {
        return;
    };
    fs::create_dir(paths.root.join(".fireworks")).unwrap();
    fs::write(
        paths.root.join(".fireworks/auth.ini"),
        "synthetic-user-state",
    )
    .unwrap();
    let exporter = fixture(&paths, "ok");
    assert_eq!(exporter.export(OPTIONS), json!({"status":"ok","csv":CSV}));
    let events = interactions(&paths);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["argv"], json!(["--help"]));
    assert_eq!(events[0]["auth_exists"], false);
    assert!(events[0]["environment"].get("FIREWORKS_API_KEY").is_none());
    let export = &events[1];
    let root = PathBuf::from(export["cwd"].as_str().unwrap());
    assert_eq!(
        export["argv"],
        json!([
            "billing",
            "export-metrics",
            "--account-id",
            "fixture-account",
            "--start-time",
            "2026-09-24",
            "--end-time",
            "2026-10-24",
            "--filename",
            format!("{}.csv", root.file_name().unwrap().to_str().unwrap())
        ])
    );
    assert_eq!(export["auth"], "[fireworks]\napi_key = selected-test-key\n");
    assert_eq!(
        export["environment"]["FIREWORKS_API_KEY"],
        "selected-test-key"
    );
    assert_eq!(export["auth_mode"], 0o600);
    assert_eq!(export["auth_dir_mode"], 0o700);
    for event in &events {
        assert_eq!(event["root_mode"], 0o700);
        for mode in event["directory_modes"].as_object().unwrap().values() {
            assert_eq!(mode, 0o700);
        }
        let env = event["environment"].as_object().unwrap();
        for key in ["FIRECTL_CONFIG", "DBUS_SESSION_BUS_ADDRESS"] {
            assert!(!env.contains_key(key));
        }
        assert_eq!(env["HTTPS_PROXY"], "http://fixture.invalid:8080");
        assert_eq!(env["NO_PROXY"], "fixture.invalid");
        assert!(
            env["SSL_CERT_FILE"]
                .as_str()
                .unwrap()
                .ends_with("synthetic-ca.pem")
        );
        for key in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
            "TMPDIR",
            "TEMPDIR",
        ] {
            assert!(PathBuf::from(env[key].as_str().unwrap()).starts_with(&root));
        }
        assert!(
            !serde_json::to_string(&event["argv"])
                .unwrap()
                .contains("selected-test-key")
        );
    }
    assert!(!root.exists());
    assert_eq!(
        fs::read_to_string(paths.root.join(".fireworks/auth.ini")).unwrap(),
        "synthetic-user-state"
    );
    assert_clean(&paths);
}

#[test]
fn preserves_absent_runner_empty_export_and_failed_export_statuses() {
    let Some(paths) = enter(concat!(
        module_path!(),
        "::preserves_absent_runner_empty_export_and_failed_export_statuses"
    )) else {
        return;
    };
    let mut exporter = fixture(&paths, "ok");
    exporter.candidates = vec![paths.root.join("not-installed")];
    assert_eq!(exporter.export(OPTIONS), json!({"status":"no_runner"}));
    for (behavior, status) in [
        ("bad_help", "no_runner"),
        ("empty", "empty"),
        ("missing", "empty"),
        ("fail", "runner_failed"),
    ] {
        assert_eq!(
            fixture(&paths, behavior).export(OPTIONS),
            json!({"status":status}),
            "{behavior}"
        );
        assert_clean(&paths);
    }
}

#[test]
fn tries_only_candidates_until_first_successful_help() {
    let Some(paths) = enter(concat!(
        module_path!(),
        "::tries_only_candidates_until_first_successful_help"
    )) else {
        return;
    };
    let mut exporter = fixture(&paths, "ok");
    exporter.candidates.insert(0, paths.root.join("missing"));
    exporter.candidates.push(paths.root.join("never-invoke"));
    assert_eq!(exporter.export(OPTIONS), json!({"status":"ok","csv":CSV}));
    assert_eq!(interactions(&paths).len(), 2);
    assert_clean(&paths);
}

#[test]
fn bounds_discovery_export_and_descendant_pipes_and_cleans_every_workspace() {
    let Some(paths) = enter(concat!(
        module_path!(),
        "::bounds_discovery_export_and_descendant_pipes_and_cleans_every_workspace"
    )) else {
        return;
    };
    for (behavior, status) in [
        ("help_timeout", "no_runner"),
        ("timeout", "timed_out"),
        ("descendant", "timed_out"),
    ] {
        let mut exporter = fixture(&paths, behavior);
        exporter.timeout = Duration::from_secs(1);
        assert_eq!(
            exporter.export(OPTIONS),
            json!({"status":status}),
            "{behavior}"
        );
        assert_clean(&paths);
    }
    let events = interactions(&paths);
    let pid = events
        .iter()
        .find_map(|event| event.get("descendant_pid").and_then(Value::as_i64))
        .unwrap();
    // Signal delivery is asynchronous; a pidfd observes exit without assuming scheduler timing.
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if fd < 0 {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    } else {
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut poll, 1, 1000) },
            1,
            "descendant survived helper return"
        );
        assert_ne!(poll.revents & libc::POLLIN, 0);
    }
}

#[test]
fn sandbox_runtime_denies_export_without_invoking_any_candidate() {
    use crate::plugin_engine::{manifest, runtime};
    let Some(paths) = enter(concat!(
        module_path!(),
        "::sandbox_runtime_denies_export_without_invoking_any_candidate"
    )) else {
        return;
    };
    let _exporter = fixture(&paths, "ok");
    let plugin = manifest::LoadedPlugin {
        manifest: manifest::PluginManifest {
            schema_version: 1,
            id: "fireworks-fixture".into(),
            name: "Fireworks Fixture".into(),
            version: "0.0.0".into(),
            entry: "plugin.js".into(),
            icon: "icon.svg".into(),
            brand_color: None,
            lines: vec![],
            links: vec![],
        },
        plugin_dir: paths.root.clone(),
        icon_data_url: "data:image/svg+xml;base64,".into(),
        entry_script: format!(
            r#"globalThis.__openusage_plugin={{probe(ctx){{let denied=false;try{{ctx.host.fireworks.exportBillingMetrics({OPTIONS})}}catch(e){{denied=String(e).includes('disabled in sandbox')}}return {{lines:[ctx.line.text({{label:'Denied',value:String(denied)}})]}}}}}}"#
        ),
    };
    let output = runtime::run_probe_in_sandbox(
        &plugin,
        &paths.app_data,
        "test",
        Some(&paths.overrides),
        &paths.root,
    );
    assert_eq!(output.outputs.len(), 1);
    assert!(
        matches!(&output.outputs[0].lines[..],[runtime::MetricLine::Text {label,value,..}] if label=="Denied" && value=="true")
    );
    assert!(!paths.root.join("interactions.jsonl").exists());
    assert_clean(&paths);
}

#[test]
fn pinned_native_plugin_uses_real_binding_csv_for_exact_token_totals() {
    let Some(paths) = enter(concat!(
        module_path!(),
        "::pinned_native_plugin_uses_real_binding_csv_for_exact_token_totals"
    )) else {
        return;
    };
    let rt = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&rt).unwrap();
    context.with(|ctx| {
        let host=Object::new(ctx.clone()).unwrap();
        inject_exporter(&ctx,&host,Some(fixture(&paths,"ok")),false).unwrap();
        ctx.globals().set("fixtureHost",host).unwrap();
        ctx.eval::<(),_>(r#"
          globalThis.calls=[];globalThis.messages=[];
          fixtureHost.env={get(){throw Error('unexpected env read')}};
          fixtureHost.log={info(m){messages.push(m)},warn(m){messages.push(m)},error(m){messages.push(m)}};
          globalThis.fixtureCtx={host:fixtureHost,nowIso:'2026-10-23T12:00:00Z',app:{version:'test'},
            util:{providerApiKey(){return 'selected-test-key'},parseDateMs:Date.parse,
              request(opts){calls.push(opts.url);return {status:200,bodyText:JSON.stringify(opts.url.includes('/quotas?')?{quotas:[]}:{accounts:[{name:'accounts/fixture-account',state:'READY'}]})}},
              isAuthStatus(s){return s===401||s===403},tryParseJson:JSON.parse,toIso(v){return v||null}},
            line:{text(o){return Object.assign({type:'text'},o)},badge(o){return Object.assign({type:'badge'},o)}}};
        "#).unwrap();
        ctx.eval::<(),_>(include_str!("../../../vendor/crossusage/plugins/fireworks-ai/plugin.js")).unwrap();
        let result:String=ctx.eval("JSON.stringify(__openusage_plugin.probe(fixtureCtx))").unwrap();
        assert_eq!(serde_json::from_str::<Value>(&result).unwrap(),json!({"plan":null,"lines":[
            {"type":"text","label":"Serverless usage","value":"1.8K tokens","subtitle":"Last 30 days"},
            {"type":"text","label":"Prompt tokens","value":"1.45K tokens","subtitle":"Last 30 days"},
            {"type":"text","label":"Generated tokens","value":"350 tokens","subtitle":"Last 30 days"}]}));
        let calls:String=ctx.eval("JSON.stringify(calls)").unwrap();
        assert_eq!(serde_json::from_str::<Value>(&calls).unwrap(),json!(["https://api.fireworks.ai/v1/accounts?pageSize=200","https://api.fireworks.ai/v1/accounts/fixture-account/quotas?pageSize=200"]));
        let messages:String=ctx.eval("JSON.stringify(messages)").unwrap();
        assert!(!messages.contains("selected-test-key"));
    });
    assert_eq!(interactions(&paths).len(), 2);
    assert_clean(&paths);
}

#[test]
fn real_binding_validates_missing_and_nonstring_options_without_running_helper() {
    let Some(paths) = enter(concat!(
        module_path!(),
        "::real_binding_validates_missing_and_nonstring_options_without_running_helper"
    )) else {
        return;
    };
    let rt = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&rt).unwrap();
    context.with(|ctx| {
        let host = Object::new(ctx.clone()).unwrap();
        inject_exporter(&ctx, &host, Some(fixture(&paths, "ok")), false).unwrap();
        ctx.globals().set("host", host).unwrap();
        for expression in [
            "",
            "undefined",
            "null",
            "{}",
            "[]",
            "42",
            "{apiKey:true}",
            "{apiKey:' ',accountId:'a',startTime:'b',endTime:'c'}",
        ] {
            let result: String = ctx
                .eval(format!(
                    "JSON.stringify(host.fireworks.exportBillingMetrics({expression}))"
                ))
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&result).unwrap(),
                json!({"status":"invalid_opts"}),
                "{expression}"
            );
        }
    });
    assert!(!paths.root.join("interactions.jsonl").exists());
    assert_clean(&paths);
}

#[test]
fn preparation_errors_and_failed_export_never_leak_secrets_and_clean_up() {
    struct Capture(std::sync::Mutex<Vec<String>>);
    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            self.0.lock().unwrap().push(record.args().to_string());
        }
        fn flush(&self) {}
    }
    static CAPTURE: Capture = Capture(std::sync::Mutex::new(Vec::new()));
    let Some(paths) = enter(concat!(
        module_path!(),
        "::preparation_errors_and_failed_export_never_leak_secrets_and_clean_up"
    )) else {
        return;
    };
    log::set_logger(&CAPTURE).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    for behavior in ["bad_auth", "fail"] {
        assert_eq!(
            fixture(&paths, behavior).export(OPTIONS),
            json!({"status":"runner_failed"})
        );
        assert_clean(&paths);
    }
    let mut exporter = fixture(&paths, "ok");
    exporter.temp_root = paths.root.join("nonexistent-parent");
    assert_eq!(exporter.export(OPTIONS), json!({"status":"runner_failed"}));
    assert!(!exporter.temp_root.exists());
    assert_eq!(interactions(&paths).len(), 3);
    assert!(
        !CAPTURE
            .0
            .lock()
            .unwrap()
            .join("\n")
            .contains("selected-test-key")
    );
}

#[test]
fn configured_proxy_and_loopback_policy_reach_real_binding_without_inherited_conflicts() {
    let Some(paths) = enter(concat!(
        module_path!(),
        "::configured_proxy_and_loopback_policy_reach_real_binding_without_inherited_conflicts"
    )) else {
        return;
    };
    let config = crate::config::config_path().unwrap();
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        "proxy:\n  enabled: true\n  url: http://fixture-user:fixture-password@127.0.0.1:32123\n",
    )
    .unwrap();
    // Only the single-test hermetic child is changed; the parent environment is never modified.
    unsafe {
        std::env::set_var("HTTPS_PROXY", "http://wrong-environment.invalid:8080");
        std::env::set_var("HTTP_PROXY", "http://other-wrong.invalid:8081");
        std::env::set_var("ALL_PROXY", "socks5://wrong.invalid:1080");
        std::env::set_var("NO_PROXY", "*");
    }
    assert!(crate::config::get_resolved_proxy().is_some());
    fs::write(
        config,
        "proxy:\n  enabled: true\n  url: http://changed-config.invalid:8080\n",
    )
    .unwrap();
    let controlled = fixture(&paths, "ok");
    let mut exporter = Exporter::native();
    exporter.candidates = controlled.candidates;
    exporter.temp_root = controlled.temp_root;
    let rt = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&rt).unwrap();
    context.with(|ctx| {
        let host = Object::new(ctx.clone()).unwrap();
        inject_exporter(&ctx, &host, Some(exporter), false).unwrap();
        ctx.globals().set("host", host).unwrap();
        let result: String = ctx
            .eval(format!(
                "JSON.stringify(host.fireworks.exportBillingMetrics({OPTIONS}))"
            ))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap(),
            json!({"status":"ok","csv":CSV})
        );
    });
    for event in interactions(&paths) {
        let environment = event["environment"].as_object().unwrap();
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            assert_eq!(
                environment[name],
                "http://fixture-user:fixture-password@127.0.0.1:32123"
            );
        }
        for name in ["NO_PROXY", "no_proxy"] {
            assert_eq!(environment[name], "localhost,127.0.0.1,::1");
        }
    }
    assert_clean(&paths);
}

fn assert_native_proxy_environment(paths: &HermeticPaths, expected: &[(&str, &str)]) {
    let controlled = fixture(paths, "ok");
    let mut exporter = Exporter::native();
    exporter.candidates = controlled.candidates;
    exporter.temp_root = controlled.temp_root;
    assert_eq!(exporter.export(OPTIONS), json!({"status":"ok","csv":CSV}));
    for event in interactions(paths) {
        for (name, value) in expected {
            assert_eq!(event["environment"][*name], *value, "{name}");
        }
    }
    assert_clean(paths);
}

#[test]
fn disabled_config_preserves_existing_first_present_native_proxy_fallback() {
    let Some(paths) = enter(concat!(
        module_path!(),
        "::disabled_config_preserves_existing_first_present_native_proxy_fallback"
    )) else {
        return;
    };
    let config = crate::config::config_path().unwrap();
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        config,
        "proxy:\n  enabled: false\n  url: http://disabled.invalid:8080\n",
    )
    .unwrap();
    unsafe {
        std::env::set_var("HTTPS_PROXY", " http://selected-environment.invalid:8080 ");
        std::env::set_var("https_proxy", "http://wrong-lower.invalid:8081");
        std::env::set_var("HTTP_PROXY", "http://wrong-http.invalid:8082");
        std::env::set_var("NO_PROXY", "*");
    }
    assert!(crate::config::get_resolved_proxy().is_some());
    assert_native_proxy_environment(
        &paths,
        &[
            ("HTTP_PROXY", "http://selected-environment.invalid:8080"),
            ("http_proxy", "http://selected-environment.invalid:8080"),
            ("HTTPS_PROXY", "http://selected-environment.invalid:8080"),
            ("https_proxy", "http://selected-environment.invalid:8080"),
            ("ALL_PROXY", "http://selected-environment.invalid:8080"),
            ("all_proxy", "http://selected-environment.invalid:8080"),
            ("NO_PROXY", "localhost,127.0.0.1,::1"),
            ("no_proxy", "localhost,127.0.0.1,::1"),
        ],
    );
}

#[test]
fn invalid_config_falls_back_without_logging_proxy_credentials() {
    struct Capture(std::sync::Mutex<Vec<String>>);
    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            self.0.lock().unwrap().push(record.args().to_string());
        }
        fn flush(&self) {}
    }
    static CAPTURE: Capture = Capture(std::sync::Mutex::new(Vec::new()));
    let Some(paths) = enter(concat!(
        module_path!(),
        "::invalid_config_falls_back_without_logging_proxy_credentials"
    )) else {
        return;
    };
    log::set_logger(&CAPTURE).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let config = crate::config::config_path().unwrap();
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        config,
        "proxy:\n  enabled: true\n  url: 'http://fixture-user:must-not-log-password@[malformed'\n",
    )
    .unwrap();
    unsafe {
        std::env::set_var("HTTP_PROXY", "http://fallback.invalid:8080");
    }
    assert!(crate::config::get_resolved_proxy().is_some());
    assert_native_proxy_environment(
        &paths,
        &[
            ("HTTP_PROXY", "http://fallback.invalid:8080"),
            ("HTTPS_PROXY", "http://fallback.invalid:8080"),
        ],
    );
    let messages = CAPTURE.0.lock().unwrap().join("\n");
    assert!(!messages.contains("must-not-log-password"));
    assert!(!messages.contains("selected-test-key"));
}

#[test]
fn all_proxy_only_system_fallback_reaches_exporter_in_both_protocols() {
    let Some(paths) = enter(concat!(
        module_path!(),
        "::all_proxy_only_system_fallback_reaches_exporter_in_both_protocols"
    )) else {
        return;
    };
    unsafe {
        std::env::set_var("ALL_PROXY", "http://all-only.invalid:8080");
        std::env::set_var("NO_PROXY", "synthetic-internal.invalid");
    }
    assert!(crate::config::get_resolved_proxy().is_none());
    assert_native_proxy_environment(
        &paths,
        &[
            ("HTTP_PROXY", "http://all-only.invalid:8080"),
            ("http_proxy", "http://all-only.invalid:8080"),
            ("HTTPS_PROXY", "http://all-only.invalid:8080"),
            ("https_proxy", "http://all-only.invalid:8080"),
            ("ALL_PROXY", "http://all-only.invalid:8080"),
            ("all_proxy", "http://all-only.invalid:8080"),
            ("NO_PROXY", "synthetic-internal.invalid"),
            ("no_proxy", "synthetic-internal.invalid"),
        ],
    );
}
