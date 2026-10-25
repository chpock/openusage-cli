#[path = "support/hermetic.rs"]
mod hermetic;

use hermetic::HermeticPaths;
use openusage_cli::plugin_engine::{manifest, runtime};
use serde_json::{Value, json};
use std::fs;

fn probe(paths: &HermeticPaths, body: &str) -> Value {
    let plugin = manifest::LoadedPlugin {
        manifest: manifest::PluginManifest {
            schema_version: 1,
            id: "host-contract".into(),
            name: "Host Contract".into(),
            version: "0.0.0".into(),
            entry: "plugin.js".into(),
            icon: "icon.svg".into(),
            brand_color: None,
            lines: vec![],
            links: vec![],
        },
        plugin_dir: paths.root.clone(),
        entry_script: format!(
            "globalThis.__openusage_plugin={{probe(ctx){{ {body} ;return {{lines:[ctx.line.text({{label:'Result',value:JSON.stringify(result)}})]}};}}}};"
        ),
        icon_data_url: "data:image/svg+xml;base64,".into(),
    };
    let output = runtime::run_probe_in_sandbox(
        &plugin,
        &paths.app_data,
        "test",
        Some(&paths.overrides),
        &paths.root,
    );
    assert_eq!(output.outputs.len(), 1);
    match &output.outputs[0].lines[..] {
        [runtime::MetricLine::Text { label, value, .. }] if label == "Result" => {
            serde_json::from_str(value).expect("decode host result")
        }
        lines => panic!("host contract failed: {lines:?}"),
    }
}

#[test]
fn host_paths_preserve_candidate_order_and_supplied_paths() {
    let Some(paths) = hermetic::enter("host_paths_preserve_candidate_order_and_supplied_paths")
    else {
        return;
    };
    fs::create_dir_all(paths.root.join("first")).unwrap();
    fs::create_dir_all(paths.root.join("second")).unwrap();
    fs::write(paths.root.join("first/state.json"), "").unwrap();
    fs::write(paths.root.join("second/state.json"), "second").unwrap();
    let result = probe(
        &paths,
        r#"
        var result = {
            home: ctx.host.fs.homeDir,
            selected: ctx.host.fs.firstExisting(['~/missing', '~/first/state.json', '~/second/state.json']),
            directory: ctx.host.fs.firstExisting(['~/missing', '~/first']),
            absent: typeof ctx.host.fs.firstExisting(['~/missing']),
            empty: typeof ctx.host.fs.firstExisting([])
        };
    "#,
    );
    assert_eq!(
        result,
        json!({
            "home": paths.root.to_string_lossy(), "selected":"~/first/state.json",
            "directory":"~/first", "absent":"undefined", "empty":"undefined"
        })
    );
}

#[test]
fn host_app_support_discovery_uses_upstream_order() {
    let Some(paths) = hermetic::enter("host_app_support_discovery_uses_upstream_order") else {
        return;
    };
    let candidates = [
        ".config/Fixture/settings.json",
        "Library/Application Support/Fixture/settings.json",
        "AppData/Roaming/Fixture/settings.json",
    ];
    for candidate in candidates {
        let path = paths.root.join(candidate);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, candidate).unwrap();
    }
    for candidate in candidates {
        assert_eq!(
            probe(
                &paths,
                "var result=ctx.host.fs.firstExistingAppSupport('Fixture/settings.json')"
            ),
            format!("~/{candidate}")
        );
        fs::remove_file(paths.root.join(candidate)).unwrap();
    }
    assert_eq!(
        probe(
            &paths,
            "var result=typeof ctx.host.fs.firstExistingAppSupport('Fixture/settings.json')"
        ),
        "undefined"
    );
}

#[test]
fn host_nested_writes_are_private_and_preserve_overwrite_behavior() {
    let Some(paths) =
        hermetic::enter("host_nested_writes_are_private_and_preserve_overwrite_behavior")
    else {
        return;
    };
    let script = r#"
        ctx.host.fs.writeText('~/cache/new/nested/state.json', 'first');
        var first = ctx.host.fs.readText('~/cache/new/nested/state.json');
        ctx.host.fs.writeText('~/cache/new/nested/state.json', 'new');
        var result = {first:first, final:ctx.host.fs.readText('~/cache/new/nested/state.json')};
    "#;
    assert_eq!(
        probe(&paths, script),
        json!({"first":"first", "final":"new"})
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file = paths.root.join("cache/new/nested/state.json");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        probe(&paths, script);
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn host_nested_write_failures_do_not_mask_errors_or_escape_sandbox() {
    let Some(paths) =
        hermetic::enter("host_nested_write_failures_do_not_mask_errors_or_escape_sandbox")
    else {
        return;
    };
    fs::write(paths.root.join("blocker"), "unchanged").unwrap();
    let result = probe(
        &paths,
        r#"
        function denied(path) {
            try { ctx.host.fs.writeText(path, 'changed'); return false; }
            catch (_) { return true; }
        }
        var result={blocked:denied('~/blocker/state.json'),
            traversal:denied('~/../must-not-be-created/state.json'),
            outside:typeof ctx.host.fs.firstExisting(['/etc/passwd']),
            traversalDiscovery:typeof ctx.host.fs.firstExistingAppSupport('../blocker')};
    "#,
    );
    assert_eq!(
        result,
        json!({"blocked":true, "traversal":true, "outside":"undefined", "traversalDiscovery":"undefined"})
    );
    assert_eq!(
        fs::read_to_string(paths.root.join("blocker")).unwrap(),
        "unchanged"
    );
}

#[cfg(unix)]
#[test]
fn host_nested_writes_and_discovery_deny_symlink_escape() {
    use std::os::unix::fs::symlink;
    let Some(paths) = hermetic::enter("host_nested_writes_and_discovery_deny_symlink_escape")
    else {
        return;
    };
    // TMPDIR is inside the sandbox; use a second owned fixture beside its root.
    let outside = tempfile::tempdir_in(paths.root.parent().unwrap()).unwrap();
    fs::write(outside.path().join("sentinel"), "untouched").unwrap();
    symlink(outside.path(), paths.root.join("escape")).unwrap();
    let result = probe(
        &paths,
        r#"
        var denied=false;
        try {ctx.host.fs.writeText('~/escape/new/nested/state.json','bad');}
        catch (_) {denied=true;}
        var result={denied:denied, selected:typeof ctx.host.fs.firstExisting(['~/escape/sentinel'])};
    "#,
    );
    assert_eq!(result, json!({"denied":true, "selected":"undefined"}));
    assert_eq!(
        fs::read_to_string(outside.path().join("sentinel")).unwrap(),
        "untouched"
    );
    assert!(!outside.path().join("new").exists());
}

#[test]
fn host_json_helpers_accept_initial_bom_without_weakening_errors() {
    let Some(paths) =
        hermetic::enter("host_json_helpers_accept_initial_bom_without_weakening_errors")
    else {
        return;
    };
    let result = probe(
        &paths,
        r#"
        var result={value:ctx.util.tryParseJson('\uFEFF {"key":42} '),
            safe:ctx.util.safeJsonParse('\uFEFF [0,false,null]'),
            bad:ctx.util.safeJsonParse('\uFEFF {bad}'),
            empty:ctx.util.tryParseJson('\uFEFF '),
            scalar:ctx.util.safeJsonParse('\uFEFF false')};
    "#,
    );
    assert_eq!(
        result,
        json!({"value":{"key":42}, "safe":{"ok":true,"value":[0,false,null]},
        "bad":{"ok":false}, "empty":null, "scalar":{"ok":true,"value":false}})
    );
}

#[test]
fn host_hashes_use_utf8_and_lowercase_hex() {
    let Some(paths) = hermetic::enter("host_hashes_use_utf8_and_lowercase_hex") else {
        return;
    };
    let result = probe(
        &paths,
        r#"
        var result=['','abc','Hello 🌍'].map(function(text){return {
            sha256:ctx.host.crypto.sha256Hex(text), sha1:ctx.host.crypto.sha1Hex(text)};});
    "#,
    );
    assert_eq!(
        result,
        json!([
            {"sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "sha1":"da39a3ee5e6b4b0d3255bfef95601890afd80709"},
            {"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "sha1":"a9993e364706816aba3e25717850c26c9cd0d89d"},
            {"sha256":"6e2de92e24e019c3d3e5df9f616b0472f0b9a98925b8e4e6e576172545160c4a", "sha1":"b50c11e3a51d356b83deb4a51e2b3bf86ed6fd22"}
        ])
    );
}

#[test]
fn host_process_environment_exposes_only_approved_provider_inputs() {
    let Some(paths) =
        hermetic::enter("host_process_environment_exposes_only_approved_provider_inputs")
    else {
        return;
    };
    let names = [
        "CROFAI_API_KEY",
        "COMMAND_CODE_API_KEY",
        "DEEPSEEK_API_KEY",
        "DEEPSEEK_INITIAL_BALANCE",
        "OLLAMA_API_KEY",
        "OLLAMA_HOST",
        "OLLAMA_SESSION_COOKIE",
        "OLLAMA_COOKIE",
        "GEMINI_COOKIE",
        "NEURALWATT_API_KEY",
        "FIREWORKS_API_KEY",
        "OPENROUTER_API_KEY",
        "OPENROUTER_KEY",
    ];
    let plugin_dir = paths.root.join("cli-plugins/host-contract");
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(
        plugin_dir.join("plugin.json"),
        json!({
            "schemaVersion":1,"id":"host-contract","name":"Host Contract",
            "version":"0.0.0","entry":"plugin.js","icon":"icon.svg","lines":[]
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        plugin_dir.join("icon.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
    )
    .unwrap();
    fs::write(
        plugin_dir.join("plugin.js"),
        format!(
            r#"
        globalThis.__openusage_plugin={{probe(ctx){{
            var values={{}};
            {}.forEach(function(name){{ values[name]=ctx.host.env.get(name); }});
            values.denied=typeof ctx.host.env.get('UNRELATED_SECRET');
            values.absent=typeof ctx.host.env.get('ZAI_API_KEY');
            return {{lines:[ctx.line.text({{label:'Environment',value:JSON.stringify(values)}})]}};
        }}}};
    "#,
            serde_json::to_string(&names).unwrap()
        ),
    )
    .unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_openusage-cli"));
    command
        .args([
            "query",
            "--use-daemon=false",
            "--enabled-plugins=host-contract",
        ])
        .arg("--plugins-dir")
        .arg(paths.root.join("cli-plugins"))
        .arg("--app-data-dir")
        .arg(&paths.app_data)
        .arg("--plugin-overrides-dir")
        .arg(&paths.overrides)
        .env("UNRELATED_SECRET", "must-not-be-visible");
    let mut expected = serde_json::Map::new();
    for name in names {
        command.env(name, format!("  fixture-{name}\n "));
        expected.insert(name.into(), json!(format!("fixture-{name}")));
    }
    expected.insert("denied".into(), json!("undefined"));
    expected.insert("absent".into(), json!("undefined"));
    let output = command.output().expect("run synthetic environment query");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshots: Value = serde_json::from_slice(&output.stdout).expect("decode usage output");
    let values: Value = serde_json::from_str(
        snapshots[0]["lines"][0]["value"]
            .as_str()
            .expect("environment result"),
    )
    .expect("decode environment values");
    assert_eq!(values, Value::Object(expected));
}

fn tls_negotiation_case(paths: &HermeticPaths, http1_only: bool) -> (Value, String) {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // These public test-only credentials are never trusted by the default client.
    let cert = CertificateDer::from(include_bytes!("support/tls/localhost-cert.der").to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        include_bytes!("support/tls/test-only-key.der").to_vec(),
    ));
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("configure fixture TLS");
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no TLS fixture connection");
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("TLS fixture accept failed: {error}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        while connection.is_handshaking() {
            connection
                .complete_io(&mut socket)
                .expect("fixture TLS handshake");
        }
        let protocol = String::from_utf8(connection.alpn_protocol().unwrap().to_vec()).unwrap();
        if protocol == "http/1.1" {
            let mut stream = rustls::StreamOwned::new(connection, socket);
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).expect("read fixture request");
                assert!(
                    count > 0 && request.len() < 8192,
                    "incomplete fixture request"
                );
                request.extend_from_slice(&buffer[..count]);
            }
            assert!(request.starts_with(b"GET /fixture HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
            stream.flush().unwrap();
        }
        // For h2, TLS negotiation itself is the oracle; no HTTP/2 server is needed.
        protocol
    });
    let result = probe(
        paths,
        &format!(
            r#"
        var result;
        try {{
            var response=ctx.host.http.request({{url:'https://{address}/fixture',
                timeoutMs:2000,dangerouslyIgnoreTls:true,http1Only:{http1_only}}});
            result={{ok:true,status:response.status,body:response.bodyText}};
        }} catch (error) {{result={{ok:false,error:String(error)}};}}
    "#
        ),
    );
    let protocol = server
        .join()
        .unwrap_or_else(|_| panic!("TLS fixture failed; host result: {result}"));
    (result, protocol)
}

#[test]
fn host_http1_only_controls_actual_tls_protocol_negotiation() {
    let Some(paths) = hermetic::enter("host_http1_only_controls_actual_tls_protocol_negotiation")
    else {
        return;
    };
    let (_, default_protocol) = tls_negotiation_case(&paths, false);
    assert_eq!(default_protocol, "h2");
    let (response, forced_protocol) = tls_negotiation_case(&paths, true);
    assert_eq!(forced_protocol, "http/1.1");
    assert_eq!(response, json!({"ok":true,"status":200,"body":"ok"}));
}

#[test]
fn host_http_and_plugin_logs_redact_current_provider_credentials() {
    let Some(paths) =
        hermetic::enter("host_http_and_plugin_logs_redact_current_provider_credentials")
    else {
        return;
    };
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        unsafe { std::env::remove_var(key) };
    }
    let logs = Arc::new(Mutex::new(Vec::new()));
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .format_timestamp(None)
        .target(env_logger::Target::Pipe(Box::new(Capture(Arc::clone(
            &logs,
        )))))
        .try_init()
        .unwrap();

    let cookie = "cookie-private-middle-0123456789";
    let account = "account-private-middle-0123456789";
    let devin = "devin-session-token$private-middle-0123456789";
    let email = "operator-private-middle@example.invalid";
    let identity = "identity-private-middle-0123456789";
    let body = json!({
        "note":format!("__Secure-1PSID={cookie}; theme=light"),
        "session":devin, "owner":email, "org_id":identity,
        "accountDisplayName":identity, "uuid":identity, "count":7
    });
    let response_body = body.to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "request ended before headers");
            request.extend_from_slice(&buffer[..count]);
        }
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response_body.len(), response_body).unwrap();
        String::from_utf8(request).unwrap()
    });
    let url = format!(
        "http://{address}/accounts/{account}/usage?cookie={cookie}&sid={cookie}&sapisid={cookie}&mode=total"
    );
    let message =
        format!("session __Secure-1PSID={cookie}; credential={devin}; owner={email}; count=7");
    let result = probe(
        &paths,
        &format!(
            r#"
        var response = ctx.host.http.request({{url:{url}, method:'GET', timeoutMs:5000}});
        ctx.host.log.info({message});
        var result = {{received:ctx.util.tryParseJson(response.bodyText)}};
    "#,
            url = serde_json::to_string(&url).unwrap(),
            message = serde_json::to_string(&message).unwrap()
        ),
    );
    let request = server.join().unwrap();
    assert!(request.starts_with(&format!("GET /accounts/{account}/usage?cookie={cookie}&sid={cookie}&sapisid={cookie}&mode=total HTTP/1.1\r\n")));
    assert_eq!(
        result["received"], body,
        "redaction must not alter the actual HTTP payload"
    );
    let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(output.contains("[plugin:host-contract] HTTP GET"));
    assert!(output.contains("__Secure-1PSID="));
    assert!(output.contains("count=7"));
    assert!(output.contains("&mode=total"));
    for secret in [cookie, account, devin, email, identity] {
        assert!(
            !output.contains(secret),
            "synthetic credential leaked: {output}"
        );
    }
}
