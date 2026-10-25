use super::*;
use crate::hermetic;
use crate::plugin_engine::{manifest, runtime};
use serde_json::{Value, json};

#[test]
fn sandbox_sdk_denies_remote_sqlite() {
    let Some(paths) =
        hermetic::enter("plugin_engine::cursor_remote::tests::sandbox_sdk_denies_remote_sqlite")
    else {
        return;
    };
    let db = paths
        .root
        .join(".config/Cursor Nightly/User/globalStorage/state.vscdb");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    std::fs::write(&db, "synthetic unreadable SQLite content").unwrap();
    let plugin = manifest::LoadedPlugin {
        manifest: manifest::PluginManifest { schema_version: 1, id: "cursor-nightly".into(), name: "Fixture".into(), version: "0".into(), entry: "plugin.js".into(), icon: "icon.svg".into(), brand_color: None, lines: vec![], links: vec![] },
        plugin_dir: paths.root.clone(), icon_data_url: String::new(),
        entry_script: "globalThis.__openusage_plugin={probe(ctx){return {lines:[ctx.line.text({label:'Result',value:JSON.stringify({db:ctx.host.cursorPaths.resolveStateDb(),exports:['queryMtd','queryStats','queryDaily'].map(k=>ctx.host.cursorUsageExport[k]({}))})})]}}}".into(),
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
            let values: Value = serde_json::from_str(value).unwrap();
            assert_eq!(
                values["exports"],
                json!([{"status":"error","message":"Cursor remote SQLite bridge is disabled in sandbox"},{"status":"error","message":"Cursor remote SQLite bridge is disabled in sandbox"},{"status":"error","message":"Cursor remote SQLite bridge is disabled in sandbox"}])
            );
            assert_eq!(values["db"], db.to_string_lossy().as_ref());
            assert_eq!(
                std::fs::read_to_string(&db).unwrap(),
                "synthetic unreadable SQLite content"
            );
            assert!(!paths.app_data.join("cursor-remote").exists());
        }
        other => panic!("remote SDK denial failed: {other:?}"),
    }
}

const CSV: &str = "\u{feff}Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\r\n2026-01-02,\"claude,fixture\",20,\"1,000\",30,50,0,$1.25\r\n2026-01-02,gpt-fixture,2,10,3,5,20,$0.75\r\n2026-01-01,gpt-old,0,1,0,0,1,$9\r\n";

#[test]
fn csv_native_units_and_aggregation() {
    let rows = parse_usage_csv(CSV, "20260102", "20260102").unwrap();
    let stats = render(Query::Stats, &rows, "20260102", "20260102", "provider").unwrap();
    assert_eq!(
        stats["totals"],
        json!({"key":"total","input":1010,"output":55,"cacheWrite":22,"cacheHit":33,"totalTokens":1120,"costUsd":2.0})
    );
    assert_eq!(stats["rows"][0]["key"], "anthropic");
    assert_eq!(stats["rows"][1]["key"], "openai");
    assert_eq!(
        render(Query::Daily, &rows, "20260102", "20260102", "model").unwrap(),
        json!({"daily":[{"date":"2026-01-02","inputTokens":1065,"outputTokens":55,"totalTokens":1120,"costUsd":2.0}]})
    );
    assert_eq!(
        render(Query::Mtd, &rows, "20260102", "20260102", "model").unwrap(),
        json!({"since":"20260102","until":"20260102","inputTokens":1010,"outputTokens":55,"totalTokens":1120,"costUsd":2.0})
    );
    for bad in [
        "Date,Model\n2026-01-02,a",
        &CSV.replace("$1.25", "$NaN"),
        &CSV.replace("1,000", "-1000"),
        &CSV.replace("2026-01-02", "bad-date"),
    ] {
        assert!(parse_usage_csv(bad, "20260102", "20260102").is_err());
    }
}

fn token(sub: &str, exp: i64) -> String {
    use base64::Engine;
    format!(
        "e30.{}.signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(json!({"sub":sub,"exp":exp}).to_string())
    )
}
fn database(path: &std::path::Path, access: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE IF NOT EXISTS ItemTable(key TEXT PRIMARY KEY,value TEXT);")
        .unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO ItemTable VALUES (?1,?2)",
        [ACCESS_KEY, access],
    )
    .unwrap();
}

// A sequential loopback peer observes exact requests; it never forwards traffic.
fn peer(responses: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    peer_with(responses, |_, _| {})
}
fn peer_with(
    responses: Vec<(u16, String)>,
    mut before_response: impl FnMut(usize, &str) + Send + 'static,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let handle = std::thread::spawn(move || {
        let mut requests = vec![];
        for (index, (status, body)) in responses.into_iter().enumerate() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(5))
                    }
                    other => panic!("loopback accept failed: {other:?}"),
                }
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut bytes = vec![];
            loop {
                let mut buf = [0; 4096];
                let count = stream.read(&mut buf).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buf[..count]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&bytes[..end]);
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|n| n.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(bytes).unwrap());
            before_response(index, requests.last().unwrap());
            write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        requests
    });
    (url, handle)
}
fn fixture(paths: &hermetic::HermeticPaths, url: &str) -> Backend {
    Backend {
        roots: vec![
            paths.root.join(".config"),
            paths.root.join("Library/Application Support"),
        ],
        custom_db: None,
        cache_dir: paths.app_data.join("cursor-remote"),
        policy: FilesystemPolicy::Unrestricted,
        endpoints: Endpoints {
            export: format!("{url}/export"),
            events: format!("{url}/events"),
            refresh: format!("{url}/refresh"),
        },
        timeout: std::time::Duration::from_secs(2),
        backfill_elapsed_ms: None,
    }
}
fn request(backend: Backend, plugin: &str, method: &str, options: Value) -> Value {
    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        let host = rquickjs::Object::new(ctx.clone()).unwrap();
        inject_with_backend(&ctx, &host, plugin, backend).unwrap();
        ctx.globals().set("host", host).unwrap();
        let raw: String = ctx
            .eval(format!(
                "JSON.stringify(host.cursorUsageExport.{method}({options}))"
            ))
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    })
}
#[test]
fn sdk_csv_cache_is_bound_to_db_install_and_login() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::sdk_csv_cache_is_bound_to_db_install_and_login",
    ) else {
        return;
    };
    let (url, server) = peer(vec![
        (503, "".into()),
        (200, CSV.into()),
        (503, "".into()),
        (503, "".into()),
        (200, CSV.replace("$1.25", "$2.25")),
        (503, "".into()),
        (503, "".into()),
        (503, "".into()),
        (200, CSV.replace("$1.25", "$3.25")),
    ]);
    let stable = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    let nightly = paths
        .root
        .join(".config/Cursor Nightly/User/globalStorage/state.vscdb");
    database(&stable, &token("auth|one", 4102444800));
    database(&nightly, &token("auth|one", 4102444800));
    let opts = json!({"since":"20260102","until":"20260102","group":"provider"});
    assert_eq!(
        request(fixture(&paths, &url), "cursor", "queryStats", opts.clone())["data"]["totals"]["costUsd"],
        2.0
    );
    // A new QuickJS runtime and new backend must hit the same private disk cache.
    assert_eq!(
        request(fixture(&paths, &url), "cursor", "queryDaily", opts.clone())["data"]["daily"][0]["totalTokens"],
        1120
    );
    let mut explicit = opts.clone();
    explicit["baseProviderId"] = json!("cursor-nightly");
    assert_eq!(
        request(fixture(&paths, &url), "cursor", "queryStats", explicit)["data"]["totals"]["costUsd"],
        3.0
    );
    let mut precedence = opts.clone();
    precedence["pluginId"] = json!("cursor");
    precedence["baseProviderId"] = json!("cursor-nightly");
    assert_eq!(
        request(
            fixture(&paths, &url),
            "cursor-nightly",
            "queryStats",
            precedence
        )["data"]["totals"]["costUsd"],
        2.0
    );
    assert_eq!(
        request(
            fixture(&paths, &url),
            "cursor-nightly",
            "queryStats",
            opts.clone()
        )["data"]["totals"]["costUsd"],
        3.0
    );
    database(&stable, &token("auth|two", 4102444800));
    assert_eq!(
        request(fixture(&paths, &url), "cursor", "queryStats", opts.clone())["data"]["totals"]["costUsd"],
        4.0
    );
    std::fs::remove_file(&stable).unwrap();
    assert_eq!(
        request(fixture(&paths, &url), "cursor", "queryStats", opts)["status"],
        "error"
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 9);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.starts_with("GET /export?"))
            .count(),
        3
    );
    assert!(requests[0].starts_with("POST /events "));
    assert!(requests[1].starts_with("GET /export?startDate="));
    assert!(requests[1].contains("strategy=tokens"));
    assert!(requests[1].contains(&format!(
        "WorkosCursorSessionToken=one%3A%3A{}",
        token("auth|one", 4102444800)
    )));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for entry in std::fs::read_dir(paths.app_data.join("cursor-remote")).unwrap() {
            assert_eq!(
                entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                0o600
            )
        }
    }
}

#[test]
fn event_pages_count_identical_rows_and_price_in_dollars() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::event_pages_count_identical_rows_and_price_in_dollars",
    ) else {
        return;
    };
    let timestamp = chrono::Local
        .with_ymd_and_hms(2026, 1, 2, 12, 0, 0)
        .unwrap()
        .timestamp_millis();
    let event = json!({"timestamp":timestamp.to_string(),"model":"claude-sonnet-4-20250514","tokenUsage":{"inputTokens":1000,"outputTokens":100,"cacheWriteTokens":200,"cacheReadTokens":300}});
    let page = json!({"totalUsageEventsCount":2,"usageEventsDisplay":[event]}).to_string();
    let (url, server) = peer(vec![(200, page.clone()), (200, page)]);
    database(
        &paths
            .root
            .join(".config/Cursor/User/globalStorage/state.vscdb"),
        &token("auth|one", 4102444800),
    );
    let result = request(
        fixture(&paths, &url),
        "cursor",
        "queryStats",
        json!({"since":"20260102","until":"20260102"}),
    );
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(result["data"]["totals"]["input"], 2000);
    assert_eq!(result["data"]["totals"]["totalTokens"], 3200);
    assert!((result["data"]["totals"]["costUsd"].as_f64().unwrap() - 0.01068).abs() < 1e-10);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    for (index, req) in requests.iter().enumerate() {
        let body: Value = serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["page"], index + 1);
        assert_eq!(body["pageSize"], 1000);
        assert_eq!(
            body["startDate"].as_str().unwrap().parse::<i64>().unwrap(),
            local_epoch("20260102", false).unwrap()
        );
    }
}

#[test]
fn event_invalid_rows_count_toward_pagination_without_metrics() {
    let timestamp = Local
        .with_ymd_and_hms(2026, 1, 2, 12, 0, 0)
        .unwrap()
        .timestamp_millis();
    let body=json!({"totalUsageEventsCount":5,"usageEventsDisplay":[null,{"timestamp":-1,"model":"bad"},{"timestamp":timestamp,"model":"bad","tokenUsage":{"inputTokens":"5"}},{"timestamp":timestamp,"model":"gpt-test","tokenUsage":{"inputTokens":7}},{"timestamp":timestamp,"model":"empty"}]}).to_string();
    let (total, count, rows) = event_page(&body, "2026-01-02").unwrap();
    assert_eq!((total, count, rows.len()), (5, 5, 1));
    assert_eq!(rows[0].total_tokens, 7);
    let bad=json!({"totalUsageEventsCount":1,"usageEventsDisplay":[{"timestamp":timestamp,"model":"gpt-test"}]}).to_string();
    assert!(event_page(&bad, "2026-01-03").is_err());
}

#[test]
fn date_and_option_defaults_are_native_and_invalid_inputs_fail() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::date_and_option_defaults_are_native_and_invalid_inputs_fail",
    ) else {
        return;
    };
    let backend = fixture(&paths, "http://127.0.0.1:1");
    for options in [
        json!([]),
        json!({"since":7}),
        json!({"since":"20260230"}),
        json!({"since":"20260103","until":"20260102"}),
        json!({"group":"day"}),
        json!({"pluginId":7}),
    ] {
        assert_eq!(
            request(backend.clone(), "cursor", "queryStats", options)["status"],
            "error"
        );
    }
    assert!(date("2026012").is_err());
    assert!(date("20260230").is_err());
    assert_eq!(
        local_epoch("20260102", true).unwrap() - local_epoch("20260102", false).unwrap(),
        86_399_000
    );
    assert_eq!(
        request(backend, "cursor", "queryMtd", json!({})),
        json!({"status":"error","message":MTD_ERROR})
    );
}

fn add_refresh(path: &std::path::Path, refresh: &str) {
    rusqlite::Connection::open(path)
        .unwrap()
        .execute(
            "INSERT OR REPLACE INTO ItemTable VALUES (?1,?2)",
            [REFRESH_KEY, refresh],
        )
        .unwrap();
}
#[test]
fn refresh_writes_only_selected_database_and_reuses_cache() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::refresh_writes_only_selected_database_and_reuses_cache",
    ) else {
        return;
    };
    let stable = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    let nightly = paths
        .root
        .join(".config/Cursor Nightly/User/globalStorage/state.vscdb");
    let old = token("auth|night", 1);
    let fresh = token("auth|night", 4102444800);
    database(&stable, &token("auth|stable", 4102444800));
    database(&nightly, &old);
    add_refresh(&nightly, "synthetic-refresh-secret");
    let before = read_auth(&stable).unwrap();
    let (url, server) = peer(vec![
        (200, json!({"access_token":fresh}).to_string()),
        (
            200,
            json!({"totalUsageEventsCount":0,"usageEventsDisplay":[]}).to_string(),
        ),
    ]);
    let options = json!({"since":"20260102","until":"20260102"});
    let result = request(
        fixture(&paths, &url),
        "cursor-nightly",
        "queryStats",
        options.clone(),
    );
    assert_eq!(
        result["data"]["totals"],
        json!({"key":"total","input":0,"output":0,"cacheWrite":0,"cacheHit":0,"totalTokens":0,"costUsd":0}),
        "{result}"
    );
    assert_eq!(
        read_auth(&nightly).unwrap(),
        Auth {
            access: Some(fresh.clone()),
            refresh: Some("synthetic-refresh-secret".into())
        }
    );
    assert_eq!(read_auth(&stable).unwrap(), before);
    assert_eq!(
        request(
            fixture(&paths, &url),
            "cursor-nightly",
            "queryDaily",
            options
        ),
        json!({"status":"ok","data":{"daily":[]}})
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    let body: Value = serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        body,
        json!({"grant_type":"refresh_token","client_id":"KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB","refresh_token":"synthetic-refresh-secret"})
    );
    assert!(requests[1].contains(&fresh));
}

#[test]
fn corrupt_cache_and_cross_source_cache_are_explicit_errors() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::corrupt_cache_and_cross_source_cache_are_explicit_errors",
    ) else {
        return;
    };
    let db = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    database(&db, &token("auth|one", 4102444800));
    let backend = fixture(&paths, "http://127.0.0.1:1");
    let auth = read_auth(&db).unwrap();
    let identity = backend.identity("cursor", &db, &auth).unwrap();
    backend
        .save_cache(
            "csv",
            &identity,
            "20260102",
            "20260102",
            &parse_usage_csv(CSV, "20260102", "20260102").unwrap(),
        )
        .unwrap();
    let path = backend
        .cache_path(
            "csv",
            &identity,
            "20260102",
            "20260102",
            FilesystemOperation::Write,
        )
        .unwrap();
    let data = std::fs::read(&path).unwrap();
    let mut wrong: Value = serde_json::from_slice(&data).unwrap();
    wrong["identity"] = json!("different-source");
    std::fs::write(&path, wrong.to_string()).unwrap();
    let options = json!({"since":"20260102","until":"20260102"});
    let result = request(backend.clone(), "cursor", "queryStats", options.clone());
    assert_eq!(result["message"], "Cursor usage cache source mismatch");
    std::fs::write(&path, "broken synthetic-refresh-secret").unwrap();
    let result = request(backend.clone(), "cursor", "queryStats", options.clone());
    assert_eq!(result["message"], "Invalid Cursor usage cache JSON");
    assert!(!result.to_string().contains("synthetic-refresh-secret"));
    std::fs::write(&path, data).unwrap();
    database(&db, &token("auth|two", 4102444800));
    let other_id = backend
        .identity("cursor", &db, &read_auth(&db).unwrap())
        .unwrap();
    let other_path = backend
        .cache_path(
            "csv",
            &other_id,
            "20260102",
            "20260102",
            FilesystemOperation::Write,
        )
        .unwrap();
    std::fs::copy(&path, &other_path).unwrap();
    assert_eq!(
        request(backend, "cursor", "queryStats", options)["message"],
        "Cursor usage cache source mismatch"
    );
}

#[test]
fn cache_expiry_and_custom_db_path_are_resolved_before_lookup() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::cache_expiry_and_custom_db_path_are_resolved_before_lookup",
    ) else {
        return;
    };
    let db = paths.root.join("custom/state.vscdb");
    database(&db, &token("auth|one", 4102444800));
    let (url, server) = peer(vec![(503, "".into()), (200, CSV.into())]);
    let mut backend = fixture(&paths, &url);
    backend.custom_db = Some(db.clone());
    let identity = backend
        .identity("cursor", &db, &read_auth(&db).unwrap())
        .unwrap();
    backend
        .save_cache("csv", &identity, "20260102", "20260102", &[])
        .unwrap();
    let path = backend
        .cache_path(
            "csv",
            &identity,
            "20260102",
            "20260102",
            FilesystemOperation::Write,
        )
        .unwrap();
    let mut data: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    data["fetched_ms"] = json!(Utc::now().timestamp_millis() - CACHE_TTL_MS);
    std::fs::write(&path, data.to_string()).unwrap();
    assert_eq!(
        request(
            backend.clone(),
            "cursor",
            "queryStats",
            json!({"since":"20260102","until":"20260102"})
        )["data"]["totals"]["totalTokens"],
        1120
    );
    assert_eq!(
        backend.resolve_db("cursor-nightly").unwrap(),
        Some(db.clone())
    );
    std::fs::remove_file(db).unwrap();
    assert_eq!(backend.resolve_db("cursor").unwrap(), None);
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn refresh_rejects_changed_login_and_secret_error_bodies() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::refresh_rejects_changed_login_and_secret_error_bodies",
    ) else {
        return;
    };
    let db = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    database(&db, &token("auth|one", 1));
    add_refresh(&db, "synthetic-refresh-secret");
    let changed = db.clone();
    let (url, server) = peer_with(
        vec![(
            200,
            json!({"access_token":token("auth|one",4102444800)}).to_string(),
        )],
        move |_, _| database(&changed, &token("auth|changed", 4102444800)),
    );
    let options = json!({"since":"20260102","until":"20260102"});
    assert_eq!(
        request(
            fixture(&paths, &url),
            "cursor",
            "queryStats",
            options.clone()
        )["message"],
        "Cursor account changed during token refresh"
    );
    assert_eq!(
        read_auth(&db).unwrap().access,
        Some(token("auth|changed", 4102444800))
    );
    assert_eq!(server.join().unwrap().len(), 1);
    for body in [
        json!({"shouldLogout":true,"access_token":token("auth|one",4102444800)}),
        json!({"should_logout":true}),
        json!({"error":"synthetic-refresh-secret"}),
    ] {
        database(&db, &token("auth|one", 1));
        let (url, server) = peer(vec![(200, body.to_string())]);
        let result = request(
            fixture(&paths, &url),
            "cursor",
            "queryStats",
            options.clone(),
        );
        assert_eq!(result["status"], "error");
        assert!(!result.to_string().contains("synthetic-refresh-secret"));
        assert_eq!(read_auth(&db).unwrap().access, Some(token("auth|one", 1)));
        assert_eq!(server.join().unwrap().len(), 1);
    }
    database(&db, &token("auth|one", 1));
    let (url, server) = peer(vec![(
        200,
        json!({"access_token":token("other|two",4102444800)}).to_string(),
    )]);
    assert_eq!(
        request(fixture(&paths, &url), "cursor", "queryStats", options)["message"],
        "Cursor account changed during token refresh"
    );
    assert_eq!(read_auth(&db).unwrap().access, Some(token("auth|one", 1)));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn invalid_access_with_refresh_can_recover_without_cross_source_cache() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::invalid_access_with_refresh_can_recover_without_cross_source_cache",
    ) else {
        return;
    };
    let db = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    database(&db, "invalid-token");
    add_refresh(&db, "synthetic-refresh-secret");
    let (url, server) = peer(vec![
        (
            200,
            json!({"access_token":token("auth|one",4102444800)}).to_string(),
        ),
        (
            200,
            json!({"totalUsageEventsCount":0,"usageEventsDisplay":[]}).to_string(),
        ),
    ]);
    let result = request(
        fixture(&paths, &url),
        "cursor",
        "queryDaily",
        json!({"since":"20260102","until":"20260102"}),
    );
    assert_eq!(result, json!({"status":"ok","data":{"daily":[]}}));
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn incomplete_event_pages_fall_back_to_csv_without_partial_cache() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::incomplete_event_pages_fall_back_to_csv_without_partial_cache",
    ) else {
        return;
    };
    let db = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    database(&db, &token("auth|one", 4102444800));
    let timestamp = Local
        .with_ymd_and_hms(2026, 1, 2, 12, 0, 0)
        .unwrap()
        .timestamp_millis();
    for pages in [
        vec![json!({"totalUsageEventsCount":1,"usageEventsDisplay":[]})],
        vec![
            json!({"totalUsageEventsCount":0,"usageEventsDisplay":[{"timestamp":timestamp,"model":"gpt-test","tokenUsage":{"inputTokens":99}}]}),
        ],
        vec![
            json!({"totalUsageEventsCount":2,"usageEventsDisplay":[{"timestamp":timestamp,"model":"gpt-test","tokenUsage":{"inputTokens":99}}]}),
            json!({"totalUsageEventsCount":3,"usageEventsDisplay":[]}),
        ],
    ] {
        let mut responses: Vec<_> = pages.iter().map(|v| (200, v.to_string())).collect();
        responses.push((200, CSV.into()));
        let (url, server) = peer(responses);
        let backend = fixture(&paths, &url);
        let identity = backend
            .identity("cursor", &db, &read_auth(&db).unwrap())
            .unwrap();
        let cache = backend
            .cache_path(
                "csv",
                &identity,
                "20260102",
                "20260102",
                FilesystemOperation::Write,
            )
            .unwrap();
        let _ = std::fs::remove_file(cache);
        let result = request(
            backend,
            "cursor",
            "queryStats",
            json!({"since":"20260102","until":"20260102"}),
        );
        assert_eq!(result["data"]["totals"]["input"], 1010, "{result}");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), pages.len() + 1);
        assert!(requests.last().unwrap().starts_with("GET /export?"));
    }
}

#[test]
fn proxy_is_reused_and_loopback_is_bypassed() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::proxy_is_reused_and_loopback_is_bypassed",
    ) else {
        return;
    };
    let (proxy, proxied) = peer(vec![(503, "".into()), (200, CSV.into())]);
    let config = crate::config::config_path().unwrap();
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, format!("proxy:\n  enabled: true\n  url: {proxy}\n")).unwrap();
    database(
        &paths
            .root
            .join(".config/Cursor/User/globalStorage/state.vscdb"),
        &token("auth|one", 4102444800),
    );
    let options = json!({"since":"20260102","until":"20260102"});
    let result = request(
        fixture(&paths, "http://cursor-fixture.invalid"),
        "cursor",
        "queryStats",
        options.clone(),
    );
    assert_eq!(result["data"]["totals"]["costUsd"], 2.0, "{result}");
    let requests = proxied.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("POST http://cursor-fixture.invalid/events "));
    assert!(requests[1].starts_with("GET http://cursor-fixture.invalid/export?"));
    let (direct, server) = peer(vec![(
        200,
        json!({"totalUsageEventsCount":0,"usageEventsDisplay":[]}).to_string(),
    )]);
    let mut backend = fixture(&paths, &direct);
    backend.cache_dir = paths.app_data.join("other-cache");
    assert_eq!(
        request(backend, "cursor", "queryDaily", options),
        json!({"status":"ok","data":{"daily":[]}})
    );
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn request_timeout_and_redirect_policy_do_not_leak_credentials() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::request_timeout_and_redirect_policy_do_not_leak_credentials",
    ) else {
        return;
    };
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (observed, seen) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = [0; 8192];
        let n = stream.read(&mut bytes).unwrap();
        observed
            .send(String::from_utf8_lossy(&bytes[..n]).into_owned())
            .unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let mut backend = fixture(&paths, &url);
    backend.timeout = Duration::from_millis(50);
    let error = backend
        .csv(&token("auth|one", 4102444800), "20260102", "20260102")
        .unwrap_err();
    assert_eq!(error.to_string(), "Cursor export request failed");
    assert!(
        seen.recv_timeout(Duration::from_secs(5))
            .unwrap()
            .starts_with("GET /export?")
    );
    release.send(()).unwrap();
    server.join().unwrap();
    // A redirect target is deliberately unreachable; a following client would report
    // a transport error rather than the original 302 status.
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = [0; 8192];
        let n = stream.read(&mut bytes).unwrap();
        assert!(n > 0);
        stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let error = fixture(&paths, &url)
        .csv(&token("auth|one", 4102444800), "20260102", "20260102")
        .unwrap_err();
    assert_eq!(error.to_string(), "Cursor export failed: HTTP 302");
    server.join().unwrap();
}

#[test]
fn native_cursor_and_nightly_consumers_attach_real_export_metrics() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::native_cursor_and_nightly_consumers_attach_real_export_metrics",
    ) else {
        return;
    };
    let today = Local::now().date_naive().format("%Y-%m-%d").to_string();
    let csv = CSV
        .replace("2026-01-02", &today)
        .replace("2026-01-01", "1999-01-01");
    for plugin in ["cursor", "cursor-nightly"] {
        let app = if plugin == "cursor" {
            "Cursor"
        } else {
            "Cursor Nightly"
        };
        let access = token("auth|one", 4102444800);
        database(
            &paths
                .root
                .join(format!(".config/{app}/User/globalStorage/state.vscdb")),
            &access,
        );
        let mut responses = vec![(503, String::new()); Local::now().day() as usize];
        responses.push((200, csv.clone()));
        let (url, server) = peer(responses);
        let rt = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&rt).unwrap();
        context.with(|ctx| {
            let host=rquickjs::Object::new(ctx.clone()).unwrap();inject_with_backend(&ctx,&host,plugin,fixture(&paths,&url)).unwrap();ctx.globals().set("host",host).unwrap();ctx.globals().set("access",access.clone()).unwrap();
            ctx.eval::<(),_>(r#"
                var calls=[];
                host.log={info(){},warn(){},error(){}};
                host.sqlite={query(path,sql){return JSON.stringify(sql.includes('accessToken')?[{value:access}]:[])}};
                var ctx={host:host,line:{text:o=>o,progress:o=>o},fmt:{planLabel:s=>s,dollars:n=>n/100},jwt:{decodePayload:()=>({sub:'auth|one',exp:4102444800})},util:{
                    tryParseJson:s=>JSON.parse(s),needsRefreshByExpiry:()=>false,retryOnceOnAuth:o=>o.request(access),isAuthStatus:s=>s===401,toIso:()=>null,
                    request(o){calls.push(o.url);var body={};if(o.url.endsWith('GetCurrentPeriodUsage'))body={enabled:true,planUsage:{limit:2000,totalSpend:500}};else if(o.url.endsWith('GetPlanInfo'))body={planInfo:{planName:'pro'}};return {status:200,bodyText:JSON.stringify(body)}}
                }};
            "#).unwrap();
            let source=std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("vendor/crossusage/plugins/{plugin}/plugin.js"))).unwrap();ctx.eval::<(),_>(source).unwrap();
            let raw:String=ctx.eval("JSON.stringify(__openusage_plugin.probe(ctx))").unwrap();let result:Value=serde_json::from_str(&raw).unwrap();
            let mtd=result["lines"].as_array().unwrap().iter().find(|l|l["label"]=="MTD usage").unwrap();assert_eq!(mtd["value"],"1.0K tokens in · 55 tokens out · $2.00");assert_eq!(mtd["subtitle"],"Estimated spend from Cursor usage export");
            assert_eq!(result["lines"][0]["used"],25);assert_eq!(result["plan"],"pro");
        });
        assert_eq!(
            request(fixture(&paths, &url), plugin, "queryMtd", json!({}))["data"]["totalTokens"],
            1120
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), Local::now().day() as usize + 1);
    }
}

#[test]
fn event_pricing_caps_day_model_totals_like_native_not_each_request() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::event_pricing_caps_day_model_totals_like_native_not_each_request",
    ) else {
        return;
    };
    let timestamp = Local
        .with_ymd_and_hms(2026, 1, 2, 12, 0, 0)
        .unwrap()
        .timestamp_millis();
    let event = json!({"timestamp":timestamp,"model":"claude-sonnet-4-20250514","tokenUsage":{"inputTokens":2_000_000_000u64}});
    let page = json!({"totalUsageEventsCount":2,"usageEventsDisplay":[event]}).to_string();
    let (url, server) = peer(vec![(200, page.clone()), (200, page)]);
    database(
        &paths
            .root
            .join(".config/Cursor/User/globalStorage/state.vscdb"),
        &token("auth|one", 4102444800),
    );
    let result = request(
        fixture(&paths, &url),
        "cursor",
        "queryStats",
        json!({"since":"20260102","until":"20260102"}),
    );
    assert_eq!(result["data"]["totals"]["input"], 4_000_000_000u64);
    let expected = (i32::MAX as f64) * 3.0 / 1_000_000.0;
    assert!(
        (result["data"]["totals"]["costUsd"].as_f64().unwrap() - expected).abs() < 1e-8,
        "{result}"
    );
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn resolver_uses_native_order_custom_override_and_zero_arguments() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::resolver_uses_native_order_custom_override_and_zero_arguments",
    ) else {
        return;
    };
    let mut backend = fixture(&paths, "http://127.0.0.1:1");
    backend.roots.push(paths.root.join("AppData/Roaming"));
    let candidates: Vec<_> = backend
        .roots
        .iter()
        .map(|r| r.join("Cursor Nightly/User/globalStorage/state.vscdb"))
        .collect();
    for candidate in &candidates {
        database(candidate, &token("auth|one", 4102444800));
    }
    assert_eq!(backend.resolve_db("cursor").unwrap(), None);
    let rt = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&rt).unwrap();
    for candidate in candidates {
        context.with(|ctx| {
            let host = Object::new(ctx.clone()).unwrap();
            inject_with_backend(&ctx, &host, "cursor-nightly", backend.clone()).unwrap();
            ctx.globals().set("host", host).unwrap();
            let actual: String = ctx.eval("host.cursorPaths.resolveStateDb()").unwrap();
            assert_eq!(PathBuf::from(actual), candidate);
        });
        std::fs::remove_file(candidate).unwrap();
    }
    let custom = paths.root.join("custom.vscdb");
    database(&custom, &token("auth|one", 4102444800));
    backend.custom_db = Some(custom.clone());
    assert_eq!(backend.resolve_db("cursor").unwrap(), Some(custom));
    assert_eq!(backend.resolve_db("unsupported").unwrap(), None);
}

#[test]
fn invalid_auth_and_cache_io_failures_make_no_network_requests() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::invalid_auth_and_cache_io_failures_make_no_network_requests",
    ) else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let db = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    let options = json!({"since":"20260102","until":"20260102"});
    let backend = fixture(&paths, &url);
    assert!(
        request(backend.clone(), "cursor", "queryStats", options.clone())["message"]
            .as_str()
            .unwrap()
            .contains("state.vscdb not found")
    );
    database(&db, "invalid-token");
    assert_eq!(
        request(backend.clone(), "cursor", "queryStats", options.clone())["message"],
        "Invalid Cursor JWT access token"
    );
    std::fs::write(&db, "not a database synthetic-refresh-secret").unwrap();
    assert_eq!(
        request(backend.clone(), "cursor", "queryStats", options.clone())["message"],
        "Failed to read Cursor auth state"
    );
    std::fs::remove_file(&db).unwrap();
    database(&db, &token("auth|one", 4102444800));
    std::fs::write(&backend.cache_dir, "not a directory").unwrap();
    assert_eq!(
        request(backend, "cursor", "queryStats", options)["message"],
        "Failed to read Cursor usage cache"
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[cfg(unix)]
#[test]
fn cache_writes_are_private_and_do_not_follow_secret_symlinks() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::cache_writes_are_private_and_do_not_follow_secret_symlinks",
    ) else {
        return;
    };
    use std::os::unix::fs::{PermissionsExt, symlink};
    let backend = fixture(&paths, "http://127.0.0.1:1");
    backend
        .save_cache("csv", "source", "20260102", "20260102", &[])
        .unwrap();
    assert_eq!(
        std::fs::metadata(&backend.cache_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let cache = backend
        .cache_path(
            "csv",
            "source",
            "20260102",
            "20260102",
            FilesystemOperation::Write,
        )
        .unwrap();
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o644)).unwrap();
    backend
        .save_cache("csv", "source", "20260102", "20260102", &[])
        .unwrap();
    assert_eq!(
        std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let sentinel = paths.root.join("sentinel");
    std::fs::write(&sentinel, "synthetic-secret").unwrap();
    std::fs::remove_file(&cache).unwrap();
    symlink(&sentinel, &cache).unwrap();
    assert!(
        backend
            .save_cache("csv", "source", "20260102", "20260102", &[])
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(sentinel).unwrap(),
        "synthetic-secret"
    );
}

#[cfg(unix)]
#[test]
fn refresh_remains_bound_to_resolved_db_when_alias_changes() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::refresh_remains_bound_to_resolved_db_when_alias_changes",
    ) else {
        return;
    };
    use std::os::unix::fs::symlink;
    let a = paths.root.join("one/state.vscdb");
    let b = paths.root.join("two/state.vscdb");
    let alias = paths.root.join("selected.vscdb");
    let old = token("auth|one", 1);
    let fresh = token("auth|one", 4102444800);
    for db in [&a, &b] {
        database(db, &old);
        add_refresh(db, "synthetic-refresh-secret");
    }
    symlink(&a, &alias).unwrap();
    let switched = alias.clone();
    let other = b.clone();
    let (url, server) = peer_with(
        vec![
            (200, json!({"access_token":fresh}).to_string()),
            (
                200,
                json!({"totalUsageEventsCount":0,"usageEventsDisplay":[]}).to_string(),
            ),
        ],
        move |index, _| {
            if index == 0 {
                std::fs::remove_file(&switched).unwrap();
                symlink(&other, &switched).unwrap();
            }
        },
    );
    let mut backend = fixture(&paths, &url);
    backend.custom_db = Some(alias);
    let result = request(
        backend,
        "cursor",
        "queryDaily",
        json!({"since":"20260102","until":"20260102"}),
    );
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(read_auth(&a).unwrap().access, Some(fresh));
    assert_eq!(read_auth(&b).unwrap().access, Some(old));
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn stats_and_daily_refresh_recent_days_but_reuse_complete_older_days() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::stats_and_daily_refresh_recent_days_but_reuse_complete_older_days",
    ) else {
        return;
    };
    let today = Local::now().date_naive();
    let old = today - chrono::Duration::days(2);
    let body = |day: NaiveDate, input: u64| {
        let timestamp = Local
            .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .timestamp_millis();
        json!({"totalUsageEventsCount":1,"usageEventsDisplay":[{"timestamp":timestamp,"model":"gpt-fixture","tokenUsage":{"inputTokens":input}}]}).to_string()
    };
    let responses = vec![
        (200, body(today, 10)),
        (200, body(today - chrono::Duration::days(1), 10)),
        (200, body(old, 10)),
        (200, body(today, 20)),
        (200, body(today - chrono::Duration::days(1), 20)),
    ];
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = count.clone();
    let (url, server) = peer_with(responses, move |_, _| {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    database(
        &paths
            .root
            .join(".config/Cursor/User/globalStorage/state.vscdb"),
        &token("auth|one", 4102444800),
    );
    let options = json!({"since":old.format("%Y%m%d").to_string(),"until":today.format("%Y%m%d").to_string()});
    let result = request(
        fixture(&paths, &url),
        "cursor",
        "queryStats",
        options.clone(),
    );
    assert_eq!(result["data"]["totals"]["input"], 30, "{result}");
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
    let result = request(fixture(&paths, &url), "cursor", "queryDaily", options);
    assert_eq!(
        result["data"]["daily"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["inputTokens"].as_u64().unwrap())
            .sum::<u64>(),
        50,
        "{result}"
    );
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 5);
    assert_eq!(server.join().unwrap().len(), 5);
}

#[test]
fn incomplete_old_event_days_refetch_and_do_not_publish_partial_yesterday() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::incomplete_old_event_days_refetch_and_do_not_publish_partial_yesterday",
    ) else {
        return;
    };
    let today = Local::now().date_naive();
    let old = today - chrono::Duration::days(2);
    let yesterday = today - chrono::Duration::days(1);
    let db = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    database(&db, &token("auth|one", 4102444800));
    let timestamp = Local
        .from_local_datetime(&old.and_hms_opt(12, 0, 0).unwrap())
        .earliest()
        .unwrap()
        .timestamp_millis();
    let page=json!({"totalUsageEventsCount":1,"usageEventsDisplay":[{"timestamp":timestamp,"model":"gpt-fixture","tokenUsage":{"inputTokens":7}}]}).to_string();
    let csv = CSV.replace("2026-01-02", &yesterday.format("%Y-%m-%d").to_string());
    let (url, server) = peer(vec![(200, page), (503, String::new()), (200, csv)]);
    let backend = fixture(&paths, &url);
    let identity = backend
        .identity("cursor", &db, &read_auth(&db).unwrap())
        .unwrap();
    let mut days = backend.load_days(&identity).unwrap();
    for day in [old, yesterday] {
        days.days.insert(
            day.format("%Y-%m-%d").to_string(),
            cache::CachedDay {
                complete: false,
                rows: vec![Row {
                    day: day.format("%Y-%m-%d").to_string(),
                    model: "gpt-fixture".into(),
                    input: 999,
                    total_tokens: 999,
                    ..Row::default()
                }],
            },
        );
    }
    backend.save_days(&days).unwrap();
    let options =
        json!({"since":old.format("%Y%m%d").to_string(),"until":old.format("%Y%m%d").to_string()});
    assert_eq!(
        request(backend.clone(), "cursor", "queryStats", options.clone())["data"]["totals"]["input"],
        7
    );
    assert_eq!(
        request(backend.clone(), "cursor", "queryStats", options)["data"]["totals"]["input"],
        7
    );
    let options = json!({"since":yesterday.format("%Y%m%d").to_string(),"until":yesterday.format("%Y%m%d").to_string()});
    assert_eq!(
        request(backend, "cursor", "queryDaily", options)["data"]["daily"][0]["inputTokens"],
        1065
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].starts_with("GET /export?"));
}

#[test]
fn event_day_cache_separates_install_path_and_login_in_fresh_contexts() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::event_day_cache_separates_install_path_and_login_in_fresh_contexts",
    ) else {
        return;
    };
    let timestamp = Local
        .with_ymd_and_hms(2026, 1, 2, 12, 0, 0)
        .unwrap()
        .timestamp_millis();
    let responses=(1..=4).map(|n|(200,json!({"totalUsageEventsCount":1,"usageEventsDisplay":[{"timestamp":timestamp,"model":"gpt-fixture","tokenUsage":{"inputTokens":n*10}}]}).to_string())).collect();
    let (url, server) = peer(responses);
    let stable = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    let nightly = paths
        .root
        .join(".config/Cursor Nightly/User/globalStorage/state.vscdb");
    let other = paths.root.join("other/state.vscdb");
    database(&stable, &token("auth|one", 4102444800));
    database(&nightly, &token("auth|two", 4102444800));
    database(&other, &token("auth|two", 4102444800));
    let options = json!({"since":"20260102","until":"20260102"});
    assert_eq!(
        request(
            fixture(&paths, &url),
            "cursor",
            "queryStats",
            options.clone()
        )["data"]["totals"]["input"],
        10
    );
    database(&stable, &token("auth|two", 4102444800));
    assert_eq!(
        request(
            fixture(&paths, &url),
            "cursor",
            "queryStats",
            options.clone()
        )["data"]["totals"]["input"],
        20
    );
    assert_eq!(
        request(
            fixture(&paths, &url),
            "cursor-nightly",
            "queryStats",
            options.clone()
        )["data"]["totals"]["input"],
        30
    );
    let mut backend = fixture(&paths, &url);
    backend.custom_db = Some(other);
    assert_eq!(
        request(backend, "cursor", "queryStats", options)["data"]["totals"]["input"],
        40
    );
    assert_eq!(server.join().unwrap().len(), 4);
}

fn budget_page(input: u64, total: u64) -> String {
    let timestamp = Local
        .with_ymd_and_hms(2026, 1, 2, 12, 0, 0)
        .unwrap()
        .timestamp_millis();
    json!({"totalUsageEventsCount":total,"usageEventsDisplay":[{"timestamp":timestamp,"model":"gpt-fixture","tokenUsage":{"inputTokens":input}}]}).to_string()
}

#[test]
fn events_budget_stops_between_days_and_falls_back_for_incomplete_window() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::events_budget_stops_between_days_and_falls_back_for_incomplete_window",
    ) else {
        return;
    };
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let elapsed = Arc::new(AtomicU64::new(0));
    let clock = elapsed.clone();
    let (url, server) = peer_with(
        vec![(200, budget_page(7, 1)), (200, CSV.into())],
        move |index, _| {
            if index == 0 {
                clock.store(25_000, Ordering::SeqCst);
            }
        },
    );
    let db = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    database(&db, &token("auth|one", 4102444800));
    let mut backend = fixture(&paths, &url);
    backend.backfill_elapsed_ms = Some(elapsed.clone());
    let result = request(
        backend.clone(),
        "cursor",
        "queryStats",
        json!({"since":"20260101","until":"20260102"}),
    );
    assert_eq!(result["data"]["totals"]["totalTokens"], 1121, "{result}");
    assert_eq!(result["data"]["totals"]["costUsd"], 11.0);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("POST /events "));
    assert!(requests[1].starts_with("GET /export?"));
    let identity = backend
        .identity("cursor", &db, &read_auth(&db).unwrap())
        .unwrap();
    let cached = backend.load_days(&identity).unwrap();
    assert_eq!(cached.days.len(), 1);
    assert_eq!(cached.days["2026-01-02"].rows[0].input, 7);
    assert!(!cached.days.contains_key("2026-01-01"));
    assert_eq!(elapsed.load(Ordering::SeqCst), 25_000);
    eprintln!(
        "events-only budget: event_requests=1 fetched_days=1 csv_requests=1 virtual_elapsed_ms=25000"
    );
}

#[test]
fn events_budget_allows_inflight_pagination_and_reuses_complete_days() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::events_budget_allows_inflight_pagination_and_reuses_complete_days",
    ) else {
        return;
    };
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let elapsed = Arc::new(AtomicU64::new(0));
    let clock = elapsed.clone();
    let (url, server) = peer_with(
        vec![(200, budget_page(7, 2)), (200, budget_page(11, 2))],
        move |index, _| {
            if index == 0 {
                clock.store(25_000, Ordering::SeqCst);
            }
        },
    );
    let db = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    database(&db, &token("auth|one", 4102444800));
    let mut backend = fixture(&paths, &url);
    backend.backfill_elapsed_ms = Some(elapsed);
    let identity = backend
        .identity("cursor", &db, &read_auth(&db).unwrap())
        .unwrap();
    let mut cached = backend.load_days(&identity).unwrap();
    cached.days.insert(
        "2026-01-01".into(),
        cache::CachedDay {
            complete: true,
            rows: vec![Row {
                day: "2026-01-01".into(),
                model: "gpt-fixture".into(),
                input: 5,
                total_tokens: 5,
                ..Row::default()
            }],
        },
    );
    backend.save_days(&cached).unwrap();
    let result = request(
        backend,
        "cursor",
        "queryStats",
        json!({"since":"20260101","until":"20260102"}),
    );
    assert_eq!(result["data"]["totals"]["input"], 23, "{result}");
    assert_eq!(result["data"]["totals"]["totalTokens"], 23);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|r| r.starts_with("POST /events ")));
    for (index, request) in requests.iter().enumerate() {
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["page"], index + 1);
        assert_eq!(
            body["startDate"],
            local_epoch("20260102", false).unwrap().to_string()
        );
    }
    eprintln!(
        "events-only budget: event_requests=2 fetched_days=1 reused_days=1 csv_requests=0 virtual_elapsed_ms=25000"
    );
}

#[test]
fn events_budget_expires_at_exactly_twenty_seconds_without_global_deadline() {
    let Some(paths) = hermetic::enter(
        "plugin_engine::cursor_remote::tests::events_budget_expires_at_exactly_twenty_seconds_without_global_deadline",
    ) else {
        return;
    };
    use std::sync::{Arc, atomic::AtomicU64};
    let (url, server) = peer(vec![(200, CSV.into())]);
    let db = paths
        .root
        .join(".config/Cursor/User/globalStorage/state.vscdb");
    database(&db, &token("auth|one", 4102444800));
    let mut backend = fixture(&paths, &url);
    backend.backfill_elapsed_ms = Some(Arc::new(AtomicU64::new(20_000)));
    let result = request(
        backend,
        "cursor",
        "queryDaily",
        json!({"since":"20260102","until":"20260102"}),
    );
    assert_eq!(result["data"]["daily"][0]["totalTokens"], 1120, "{result}");
    assert_eq!(result["data"]["daily"][0]["costUsd"], 2.0);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /export?"));
    eprintln!(
        "events-only budget: event_requests=0 fetched_days=0 csv_requests=1 virtual_elapsed_ms=20000"
    );
}
