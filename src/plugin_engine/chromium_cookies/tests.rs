use super::*;
use rusqlite::params;
use std::os::unix::fs::{PermissionsExt, symlink};

fn fixture(case: &str) -> Option<crate::hermetic::HermeticPaths> {
    crate::hermetic::enter(case)
}
fn opts() -> Options {
    Options {
        hosts: vec![".example.test".into()],
        names: vec!["A".into(), "B".into()],
    }
}
fn database(path: &Path, rows: &[(&str, &str, &str, &[u8])]) -> Connection {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE cookies(name TEXT, host_key TEXT, value TEXT, encrypted_value BLOB);",
    )
    .unwrap();
    for (name, host, value, encrypted) in rows {
        conn.execute(
            "INSERT INTO cookies VALUES (?1,?2,?3,?4)",
            params![name, host, value, encrypted],
        )
        .unwrap();
    }
    conn
}
fn scan(root: &Path, options: &Options, all: bool) -> Result<serde_json::Value, Error> {
    read_with(
        options,
        all,
        &[root.to_path_buf()],
        &FilesystemPolicy::sandbox(root.parent().unwrap()).unwrap(),
        || Ok(legacy_keys()),
    )
}
fn decode(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn independent_linux_cbc_vectors_and_host_prefix() {
    // PBKDF2 keys from Python hashlib; ciphertexts from OpenSSL enc -aes-128-cbc,
    // saltysalt, one iteration, IV of sixteen spaces, PKCS#7 padding.
    assert_eq!(
        derive_key(b"peanuts"),
        decode("fd621fe5a2b402539dfa147ca9272778").as_slice()
    );
    for (prefix, password, ciphertext, expected) in [
        (
            "v10",
            b"peanuts".as_slice(),
            "a8f24fa6f3723a07f1e6e649342fdce677d0d0485a91f9b83f829bf011a4ff02",
            "synthetic-session",
        ),
        (
            "v11",
            b"".as_slice(),
            "a5b0806fb31e406afc15919e654c5b6422706a21309e8bb06b0de95f99c44d9b",
            "empty-key-cookie",
        ),
        (
            "v11",
            b"synthetic-safe-storage".as_slice(),
            "278e513b0367f08bdea59da9caf9fd59dc3ad7bfb675f98a4722a6efccaa1c7a85956e126eeb095e8dc967b3c8ad273797e278a98e89fb5481f00e1b2c1a373f",
            "prefixed-session",
        ),
    ] {
        let encrypted = [prefix.as_bytes(), &decode(ciphertext)].concat();
        assert_eq!(
            decrypt(&encrypted, &[derive_key(password)], ".example.test"),
            Ok(expected.into())
        );
        assert!(decrypt(&encrypted, &[derive_key(b"wrong")], ".example.test").is_err());
    }
}

#[test]
fn unsupported_and_corrupt_are_errors_not_plaintext() {
    for (input, expected) in [
        (b"v12synthetic".as_slice(), V12),
        (b"v20synthetic".as_slice(), V20),
        (b"v10broken".as_slice(), DECRYPT),
        (b"unknown-ciphertext".as_slice(), DECRYPT),
    ] {
        assert_eq!(
            decrypt(input, &legacy_keys(), ".example.test"),
            Err(expected)
        );
    }
}

#[test]
fn validates_options_without_echoing_inputs() {
    for raw in [
        "{}",
        "null",
        r#"{"hosts":[],"names":["A"]}"#,
        r#"{"hosts":["x"],"names":[""]}"#,
        r#"{"hosts":[7],"names":["A"]}"#,
        r#"{"hosts":[""],"names":["A"]}"#,
    ] {
        assert_eq!(parse_options(raw).err(), Some(OPTIONS));
    }
    assert!(parse_options(r#"{"hosts":["x' OR 1=1 --"],"names":["A"]}"#).is_ok());
}

#[test]
fn discovery_network_preference_legacy_and_exact_filters() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::discovery_network_preference_legacy_and_exact_filters",
    ) else {
        return;
    };
    let root = p.root.join("profiles");
    database(
        &root.join("Default/Cookies"),
        &[("A", ".example.test", "stale", b"")],
    );
    database(
        &root.join("Default/Network/Cookies"),
        &[
            ("A", ".example.test", "live", b""),
            ("B", "example.test", "wrong-host", b""),
            ("C", ".example.test", "wrong-name", b""),
        ],
    );
    database(
        &root.join("Profile 2/Cookies"),
        &[("B", ".example.test", "legacy", b"")],
    );
    database(
        &root.join("Guest Profile/Cookies"),
        &[("A", ".example.test", "ignored", b"")],
    );
    let paths = candidates(
        std::slice::from_ref(&root),
        &FilesystemPolicy::sandbox(&p.root).unwrap(),
    )
    .0;
    assert_eq!(paths.len(), 2);
    assert!(!paths.contains(&root.join("Default/Cookies")));
    let maps = scan(&root, &opts(), true).unwrap();
    let entries = maps.as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .any(|x| x == &serde_json::json!({"A":"live"}))
    );
    assert!(
        entries
            .iter()
            .any(|x| x == &serde_json::json!({"B":"legacy"}))
    );
    let injection = Options {
        names: vec!["A' OR 1=1 --".into()],
        ..opts()
    };
    assert_eq!(
        scan(&root, &injection, true).unwrap(),
        serde_json::json!([])
    );
}

#[test]
fn profile_order_first_complete_most_complete_and_no_stitching() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::profile_order_first_complete_most_complete_and_no_stitching",
    ) else {
        return;
    };
    let roots: Vec<_> = (0..4).map(|i| p.root.join(format!("root{i}"))).collect();
    for (root, rows) in roots.iter().zip([
        vec![("A", ".example.test", "partial", b"".as_slice())],
        vec![("B", ".example.test", "other", b"".as_slice())],
        vec![
            ("A", ".example.test", "complete", b"".as_slice()),
            ("B", ".example.test", "same-profile", b"".as_slice()),
        ],
        vec![
            ("A", ".example.test", "later", b"".as_slice()),
            ("B", ".example.test", "later", b"".as_slice()),
        ],
    ]) {
        database(&root.join("Default/Cookies"), &rows);
    }
    let policy = FilesystemPolicy::sandbox(&p.root).unwrap();
    let run = |roots: &[PathBuf], all| {
        read_with(&opts(), all, roots, &policy, || Ok(legacy_keys())).unwrap()
    };
    assert_eq!(run(&roots[..2], false), serde_json::json!({"A":"partial"}));
    assert_eq!(
        run(&roots, false),
        serde_json::json!({"A":"complete","B":"same-profile"})
    );
    assert_eq!(
        run(&roots, true),
        serde_json::json!([{"A":"partial"},{"B":"other"},{"A":"complete","B":"same-profile"},{"A":"later","B":"later"}])
    );
    let expanded = Options {
        names: vec!["A".into(), "B".into(), "C".into()],
        ..opts()
    };
    assert_eq!(
        read_with(&expanded, false, &roots, &policy, || Ok(legacy_keys())).unwrap(),
        serde_json::json!({"A":"complete","B":"same-profile"})
    );
}

#[test]
fn absence_and_reachable_failures_are_distinct_and_safe() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::absence_and_reachable_failures_are_distinct_and_safe",
    ) else {
        return;
    };
    let root = p.root.join("profiles");
    fs::create_dir(&root).unwrap();
    assert_eq!(scan(&root, &opts(), false).unwrap(), serde_json::json!({}));
    let db = root.join("Default/Cookies");
    database(&db, &[("C", ".example.test", "not-requested", b"")]);
    assert_eq!(scan(&root, &opts(), true).unwrap(), serde_json::json!([]));
    fs::write(&db, b"secret SQL error body must not escape").unwrap();
    assert_eq!(scan(&root, &opts(), true), Err(SQL));
    fs::remove_file(&db).unwrap();
    database(&db, &[("A", ".example.test", "", b"v20secret")]);
    assert_eq!(scan(&root, &opts(), true), Err(V20));
    fs::remove_file(&db).unwrap();
    database(&db, &[("A", ".example.test", "", b"v10corrupt")]);
    assert_eq!(scan(&root, &opts(), true), Err(DECRYPT));
    fs::remove_file(&db).unwrap();
    symlink(p.root.join("missing"), &db).unwrap();
    assert_eq!(scan(&root, &opts(), true), Err(POLICY));
}

#[test]
fn private_copy_modes_wal_visibility_and_cleanup() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::private_copy_modes_wal_visibility_and_cleanup",
    ) else {
        return;
    };
    let db = p.root.join("source/Cookies");
    let conn = database(&db, &[]);
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    conn.execute(
        "INSERT INTO cookies VALUES ('A','.example.test','wal-only',X'')",
        [],
    )
    .unwrap();
    let original = fs::read(&db).unwrap();
    let wal_original = fs::read(db.with_file_name("Cookies-wal")).unwrap();
    let policy = FilesystemPolicy::sandbox(&p.root).unwrap();
    let workspace = Workspace::new(&policy).unwrap();
    let path = workspace.dir.path().to_path_buf();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    workspace.prepare(&db, &policy).unwrap();
    for name in ["Cookies", "Cookies-wal"] {
        assert_eq!(
            fs::metadata(path.join(name)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let result = query_copy(&path.join("Cookies"), &opts(), &legacy_keys()).unwrap();
    assert_eq!(result.0.get("A").map(String::as_str), Some("wal-only"));
    for entry in fs::read_dir(&path).unwrap() {
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    workspace.close().unwrap();
    assert!(!path.exists());
    assert_eq!(fs::read(&db).unwrap(), original);
    assert_eq!(
        fs::read(db.with_file_name("Cookies-wal")).unwrap(),
        wal_original
    );
    assert!(!db.with_file_name("Cookies-journal").exists());
}

#[test]
fn preparation_and_cleanup_failures_and_no_symlink_overwrite() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::preparation_and_cleanup_failures_and_no_symlink_overwrite",
    ) else {
        return;
    };
    let policy = FilesystemPolicy::sandbox(&p.root).unwrap();
    let db = p.root.join("source/Cookies");
    database(&db, &[]);
    let workspace = Workspace::new(&policy).unwrap();
    let path = workspace.dir.path().to_path_buf();
    let sentinel = p.root.join("sentinel");
    fs::write(&sentinel, "unchanged").unwrap();
    symlink(&sentinel, path.join("Cookies")).unwrap();
    assert_eq!(workspace.prepare(&db, &policy), Err(POLICY));
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "unchanged");
    workspace.close().unwrap();
    assert!(!path.exists());
    let workspace = Workspace::new(&policy).unwrap();
    let path = workspace.dir.path().to_path_buf();
    fs::remove_dir(&path).unwrap();
    fs::write(&path, "not a directory").unwrap();
    assert_eq!(workspace.close(), Err(CLEANUP));
    fs::remove_file(path).unwrap();
    let workspace = Workspace::new(&policy).unwrap();
    fs::create_dir(db.with_file_name("Cookies-wal")).unwrap();
    assert_eq!(workspace.prepare(&db, &policy), Err(PREPARE));
    workspace.close().unwrap();
}

#[test]
fn positive_sdk_binding_uses_controlled_reader_and_lazy_registration() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::positive_sdk_binding_uses_controlled_reader_and_lazy_registration",
    ) else {
        return;
    };
    let root = p.root.join("browser");
    database(
        &root.join("Default/Cookies"),
        &[("A", ".example.test", "bound", b"")],
    );
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    context.with(|ctx| {
        let host = Object::new(ctx.clone()).unwrap();
        let observed = calls.clone();
        let policy = FilesystemPolicy::sandbox(&p.root).unwrap();
        inject_reader(&ctx, &host, false, move |options, all| {
            observed.set(observed.get()+1);
            read_with(options, all, std::slice::from_ref(&root), &policy, || Ok(legacy_keys()))
        }).unwrap();
        assert_eq!(calls.get(), 0);
        ctx.globals().set("host", host).unwrap();
        let json: String = ctx.eval(r#"JSON.stringify([host.chromiumCookies.read({hosts:['.example.test'],names:['A','B']}),host.chromiumCookies.readProfiles({hosts:['.example.test'],names:['A','B']})])"#).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&json).unwrap(), serde_json::json!([{"A":"bound"},[{"A":"bound"}]]));
        assert!(ctx.eval::<(),_>("host.chromiumCookies.read({hosts:[],names:['A']})").is_err());
        ctx.catch();
        assert_eq!(calls.get(), 2);
    });
}

#[test]
fn sandbox_denies_both_methods_before_reader_or_filesystem_io() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::sandbox_denies_both_methods_before_reader_or_filesystem_io",
    ) else {
        return;
    };
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    context.with(|ctx| {
        let host = Object::new(ctx.clone()).unwrap();
        inject_reader(&ctx, &host, true, |_,_| panic!("sandbox invoked reader")).unwrap();
        ctx.globals().set("host", host).unwrap();
        let denied: bool = ctx.eval(r#"['read','readProfiles'].every(m => {try {host.chromiumCookies[m]({hosts:['x'],names:['A']});return false} catch(e) {return String(e).includes('disabled')}})"#).unwrap();
        assert!(denied);
    });
    // Exercise production registration through the real sandboxed probe lifecycle.
    let plugin = load_sandbox_plugin(&p.root);
    let output =
        super::super::runtime::run_probe_in_sandbox(&plugin, &p.app_data, "test", None, &p.root);
    assert!(
        matches!(&output.outputs[0].lines[..], [super::super::runtime::MetricLine::Text {value,..}] if value == "denied"),
        "{:?}",
        output.outputs
    );
}

fn load_sandbox_plugin(root: &Path) -> super::super::manifest::LoadedPlugin {
    let dir = root.join("plugins/browser-test");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("icon.svg"),
        "<svg xmlns='http://www.w3.org/2000/svg'/>",
    )
    .unwrap();
    fs::write(dir.join("plugin.json"), r#"{"schemaVersion":1,"id":"browser-test","name":"Browser test","version":"1","entry":"plugin.js","icon":"icon.svg","lines":[]}"#).unwrap();
    fs::write(dir.join("plugin.js"), r#"globalThis.__openusage_plugin={id:'browser-test',probe:function(ctx){for (const m of ['read','readProfiles']) {try {ctx.host.chromiumCookies[m]({hosts:['x'],names:['A']});throw new Error('allowed')} catch(e){if(!String(e).includes('disabled'))throw e}}return {lines:[ctx.line.text({label:'Result',value:'denied'})]}}};"#).unwrap();
    super::super::manifest::load_plugins_from_dir(&root.join("plugins")).remove(0)
}

#[test]
fn encrypted_sqlite_rows_flow_through_sdk_without_cross_profile_merging() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::encrypted_sqlite_rows_flow_through_sdk_without_cross_profile_merging",
    ) else {
        return;
    };
    let root = p.root.join("encrypted-browser");
    let v10 = [
        b"v10".as_slice(),
        &decode("a8f24fa6f3723a07f1e6e649342fdce677d0d0485a91f9b83f829bf011a4ff02"),
    ]
    .concat();
    let v11 = [b"v11".as_slice(), &decode("278e513b0367f08bdea59da9caf9fd59dc3ad7bfb675f98a4722a6efccaa1c7a85956e126eeb095e8dc967b3c8ad273797e278a98e89fb5481f00e1b2c1a373f")].concat();
    database(
        &root.join("Default/Cookies"),
        &[
            (
                "A",
                ".example.test",
                "must-not-use-plaintext-if-encrypted",
                &v10,
            ),
            ("B", ".example.test", "", &v11),
        ],
    );
    let policy = FilesystemPolicy::sandbox(&p.root).unwrap();
    let rt = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&rt).unwrap();
    context.with(|ctx| {
        let host = Object::new(ctx.clone()).unwrap();
        inject_reader(&ctx, &host, false, move |options, all| {
            read_with(options, all, std::slice::from_ref(&root), &policy, || {
                let mut keys = legacy_keys();
                keys.insert(0, derive_key(b"synthetic-safe-storage"));
                Ok(keys)
            })
        }).unwrap();
        ctx.globals().set("host", host).unwrap();
        let result: String = ctx.eval("JSON.stringify(host.chromiumCookies.read({hosts:['.example.test'],names:['A','B']}))").unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&result).unwrap(), serde_json::json!({"A":"synthetic-session","B":"prefixed-session"}));
    });
}

#[test]
fn profile_and_row_failures_allow_other_safe_results() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::profile_and_row_failures_allow_other_safe_results",
    ) else {
        return;
    };
    let bad = p.root.join("bad");
    let good = p.root.join("good");
    fs::create_dir_all(bad.join("Default")).unwrap();
    fs::write(bad.join("Default/Cookies"), "not SQLite").unwrap();
    database(
        &good.join("Default/Cookies"),
        &[
            ("A", ".example.test", "usable", b""),
            ("B", ".example.test", "", b"v12not-supported"),
        ],
    );
    let policy = FilesystemPolicy::sandbox(&p.root).unwrap();
    let roots = [bad, good];
    assert_eq!(
        read_with(&opts(), true, &roots, &policy, || Ok(legacy_keys())).unwrap(),
        serde_json::json!([{"A":"usable"}])
    );
    assert_eq!(
        read_with(&opts(), false, &roots, &policy, || Ok(legacy_keys())).unwrap(),
        serde_json::json!({"A":"usable"})
    );
}

#[test]
fn source_sidecar_and_workspace_policy_and_error_cleanup() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::source_sidecar_and_workspace_policy_and_error_cleanup",
    ) else {
        return;
    };
    let root = p.root.join("browser");
    let db = root.join("Default/Cookies");
    database(&db, &[]);
    let restricted = FilesystemPolicy::sandbox(&root).unwrap();
    assert!(matches!(Workspace::new(&restricted), Err(POLICY)));
    let dir_count = || fs::read_dir(p.root.join("tmp")).unwrap().count();
    assert_eq!(dir_count(), 0);
    let outside = p.root.join("outside-wal");
    fs::write(&outside, "not a WAL").unwrap();
    symlink(&outside, db.with_file_name("Cookies-wal")).unwrap();
    let private_wal = root.join("private-wal");
    assert_eq!(
        copy_source(
            &db.with_file_name("Cookies-wal"),
            &private_wal,
            &restricted,
            true
        ),
        Err(POLICY)
    );
    assert!(!private_wal.exists());
    let private_db = root.join("private-db");
    symlink(&outside, root.join("outside-db")).unwrap();
    assert_eq!(
        copy_source(&root.join("outside-db"), &private_db, &restricted, false),
        Err(POLICY)
    );
    assert!(!private_db.exists());
    fs::remove_file(db.with_file_name("Cookies-wal")).unwrap();
    fs::write(&db, b"bad schema").unwrap();
    assert_eq!(
        read_database(
            &db,
            &opts(),
            &legacy_keys(),
            &FilesystemPolicy::sandbox(&p.root).unwrap()
        ),
        Err(SQL)
    );
    assert_eq!(dir_count(), 0, "SQL failure leaked a private workspace");
    assert_eq!(fs::read_to_string(outside).unwrap(), "not a WAL");
}

#[test]
fn native_browser_roots_are_used_without_user_environment_or_bus() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::native_browser_roots_are_used_without_user_environment_or_bus",
    ) else {
        return;
    };
    let roots = browser_roots();
    assert_eq!(roots.len(), 13);
    assert_eq!(roots[0], p.root.join(".config/google-chrome"));
    assert_eq!(roots[3], p.root.join(".config/chromium"));
    assert_eq!(roots[6], p.root.join("snap/chromium/common/chromium"));
    database(
        &roots[0].join("Default/Network/Cookies"),
        &[("A", ".example.test", "chrome", b"")],
    );
    database(
        &roots[3].join("Profile 1/Cookies"),
        &[("B", ".example.test", "chromium", b"")],
    );
    let policy = FilesystemPolicy::sandbox(&p.root).unwrap();
    assert_eq!(
        read_with(&opts(), true, &roots, &policy, || Ok(legacy_keys())).unwrap(),
        serde_json::json!([{"A":"chrome"},{"B":"chromium"}])
    );
}

#[test]
fn native_gemini_and_correction_consume_real_profile_maps() {
    let Some(p) = fixture(
        "plugin_engine::chromium_cookies::tests::native_gemini_and_correction_consume_real_profile_maps",
    ) else {
        return;
    };
    let earlier = p.root.join("earlier-browser");
    let later = p.root.join("later-browser");
    database(
        &earlier.join("Default/Cookies"),
        &[
            ("SID", ".google.com", "partial", b""),
            ("HSID", ".google.com", "partial", b""),
            ("SSID", ".google.com", "partial", b""),
            ("APISID", ".google.com", "partial", b""),
            ("SAPISID", ".google.com", "partial", b""),
            ("__Secure-1PSIDTS", ".google.com", "partial", b""),
            ("__Secure-3PSID", ".google.com", "partial", b""),
            ("__Secure-1PAPISID", ".google.com", "partial", b""),
        ],
    );
    database(
        &later.join("Default/Cookies"),
        &[
            ("SAPISID", ".google.com", "valid-sap", b""),
            ("__Secure-1PSID", ".google.com", "valid-psid", b""),
        ],
    );
    for corrected in [false, true] {
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        context.with(|ctx| {
            let subscriptions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            super::super::host_api::inject_host_api_in_sandbox(
                &ctx,
                "gemini-apps",
                &p.app_data,
                "test",
                subscriptions,
                &p.root,
            )
            .unwrap();
            super::super::host_api::patch_http_wrapper(&ctx).unwrap();
            super::super::host_api::inject_utils(&ctx).unwrap();
            let probe_ctx: Object = ctx.globals().get("__openusage_ctx").unwrap();
            let host: Object = probe_ctx.get("host").unwrap();
            let roots = [earlier.clone(), later.clone()];
            let policy = FilesystemPolicy::sandbox(&p.root).unwrap();
            // Only discovery roots and keys are synthetic; the binding, private
            // database reader, native caller and override lifecycle are real.
            inject_reader(&ctx, &host, false, move |options, all| {
                read_with(options, all, &roots, &policy, || Ok(legacy_keys()))
            })
            .unwrap();
            ctx.eval::<(), _>(
                r#"
                globalThis.calls = [];
                __openusage_ctx.host.http.request = function(request) {
                    calls.push(request);
                    if (request.method === 'GET') return {status:200, bodyText:
                        '{"SNlM0e":"synthetic-at","cfb2h":"synthetic-build","FdrFJe":"99"}'};
                    if (request.method !== 'POST' || !request.url.startsWith('https://gemini.google.com/'))
                        throw new Error('unexpected synthetic request');
                    var rpc = request.url.includes('rpcids=jSf9Qc') ? 'jSf9Qc' : 'sJBwce';
                    var payload = rpc === 'jSf9Qc'
                        ? [2, [[35531,0.2656,2,[[1788254602]]],[2093,0.13,1,[[1788200602]]]]]
                        : ['Pro'];
                    return {status:200, bodyText:JSON.stringify([['wrb.fr',rpc,JSON.stringify(payload)]])};
                };
                "#,
            )
            .unwrap();
            super::super::override_lifecycle::run_lifecycle(
                &ctx,
                "gemini-apps",
                include_str!("../../../vendor/crossusage/plugins/gemini-apps/plugin.js"),
                corrected.then_some(include_str!("../../../plugin-overrides/gemini-apps.js")),
                None,
                None,
            )
            .unwrap();
            let raw: String = ctx
                .eval(
                    r#"JSON.stringify((function() {
                        try { return {result:__openusage_plugin.probe(__openusage_ctx), calls:calls}; }
                        catch(e) { return {error:String(e), calls:calls}; }
                    })())"#,
                )
                .unwrap();
            let output: serde_json::Value = serde_json::from_str(&raw).unwrap();
            if !corrected {
                assert!(output["error"].as_str().unwrap().contains("auth missing"));
                assert_eq!(output["calls"], serde_json::json!([]));
                return;
            }
            let result = &output["result"];
            assert_eq!(result["plan"], "Pro", "{output}");
            assert_eq!(result["lines"][0]["type"], "progress");
            assert_eq!(result["lines"][0]["label"], "Current");
            assert_eq!(result["lines"][0]["used"], 13.0);
            assert_eq!(result["lines"][1]["label"], "Weekly");
            assert_eq!(result["lines"][1]["used"], 27.0);
            assert_eq!(result["lines"][2]["value"], "Chrome");
            let calls = output["calls"].as_array().unwrap();
            assert_eq!(calls.len(), 3);
            for call in calls {
                assert_eq!(call["headers"]["Cookie"], "SAPISID=valid-sap; __Secure-1PSID=valid-psid");
                assert_eq!(call["http1Only"], true);
                assert_eq!(call["timeoutMs"], 15000);
            }
        });
    }
}
