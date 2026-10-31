use super::hermetic::HermeticPaths;
use openusage_cli::plugin_engine::{manifest, runtime};
use serde_json::{Value, json};
use std::{fs, path::Path};

pub const AUTH: &str = ".local/share/opencode/auth.json";
pub const PI_AUTH: &str = ".pi/agent/auth.json";

pub fn write(paths: &HermeticPaths, relative: &str, text: &str) {
    let path = paths.root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

pub fn auth() -> Value {
    json!({
        "anthropic": {"type":"oauth", "access":"oc-claude", "refresh":"oc-refresh", "expires":4102444800000_u64},
        "openrouter": {"type":"api", "key":"oc-router"},
        "openai": {"type":"oauth", "access":"oc-codex", "refresh":"oc-codex-refresh", "expires":4102444800000_u64, "accountId":"oc-account"},
        "github-copilot": {"type":"oauth", "access":"oc-copilot"},
        "unrelated": {"type":"api", "key":"untouched"}
    })
}

pub fn install(paths: &HermeticPaths, id: &str, options: Value) -> manifest::LoadedPlugin {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut plugin = manifest::load_plugins_from_dir(&root.join("vendor/crossusage/plugins"))
        .into_iter()
        .find(|plugin| plugin.manifest.id == id)
        .unwrap();
    let source = root.join("plugin-overrides").join(format!("{id}.js"));
    if source.exists() {
        fs::copy(source, paths.overrides.join(format!("{id}.js"))).unwrap();
    }
    let observation = paths.root.join("observed.json");
    let harness = format!(
        r#";(function () {{
            var ctx = globalThis.__openusage_ctx;
            var options = {options}, path = {path};
            var authPath = options.authPath || '~/.local/share/opencode/auth.json';
            var providerKey = options.providerKey || 'anthropic';
            var prefix = options.prefix || 'oc';
            var observed = {{requests: [], writes: [], subscriptions: [], localUsage: [], locks: [], violations: []}};
            var write = ctx.host.fs.writeText;
            var read = ctx.host.fs.readText;
            var subscribe = ctx.host.fs.subscribeFile;
            var withLock = ctx.host.fs.withAuthFileLock;
            var lockCount = 0;
            ctx.host.fs.withAuthFileLock = function(file, operation) {{
                if (file === authPath && ++lockCount === 2 && options.updateBeforeLock) {{
                    withLock(file, function() {{
                        var doc = JSON.parse(read(file));
                        doc[providerKey].access = 'pi-latest';
                        doc[providerKey].refresh = 'pi-latest-refresh';
                        doc[providerKey].expires = options.updatedExpired ? 0 : 4102444800000;
                        write(file, JSON.stringify(doc));
                    }});
                }}
                return withLock(file, operation);
            }};
            function persist() {{ write(path, JSON.stringify(observed)); }}
            ctx.host.fs.subscribeFile = function (file) {{
                observed.subscriptions.push(file); persist();
                return subscribe(file);
            }};
            ctx.host.fs.writeText = function (file, text) {{
                observed.writes.push({{path:file, text:text}}); persist();
                if (options.writeFailure && file === authPath) throw new Error('fixture write failure');
                return write(file, text);
            }};
            ctx.host.keychain = ctx.host.keychain || {{}};
            ctx.host.ccusage = ctx.host.ccusage || {{}};
            ctx.host.keychain.readGenericPassword = function () {{ return null; }};
            ctx.host.keychain.readGenericPasswordForCurrentUser = function () {{ return null; }};
            ctx.host.keychain.writeGenericPassword = function () {{
                observed.violations.push('keychain write'); persist();
                throw new Error('unexpected keychain write');
            }};
            ctx.host.ccusage.query = function (opts) {{
                observed.localUsage.push(opts); persist();
                return {{status:'ok', data:{{daily:[{{date:new Date().toISOString().slice(0,10), totalTokens:100, totalCost:1}}]}}}};
            }};
            ctx.host.http.request = function (request) {{
                observed.requests.push(request); persist();
                var url = request.url;
                if (url === 'https://platform.claude.com/v1/oauth/token' || url === 'https://auth.openai.com/oauth/token') {{
                    observed.locks.push({{path:authPath, held:ctx.host.fs.exists(authPath + '.lock')}}); persist();
                    if (options.changeDuringRefresh) {{
                        var doc = JSON.parse(read(authPath));
                        doc.concurrentSibling = {{key:'preserve-latest'}};
                        write(authPath, JSON.stringify(doc));
                    }}
                    if (options.corruptDuringRefresh) write(authPath, '{{bad');
                    if (options.replaceLoginDuringRefresh) {{
                        var doc = JSON.parse(read(authPath));
                        doc[providerKey] = Object.assign({{}}, doc[providerKey], {{type:'oauth',access:'different-login',refresh:'different-refresh',expires:4102444800000}});
                        write(authPath, JSON.stringify(doc));
                    }};
                    var body = {{access_token:prefix + '-renewed', expires_in:options.invalidExpiry ? 'invalid' : 3600}};
                    if (!options.noRotation) body.refresh_token = prefix + '-rotated';
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify(body)}};
                }}
                var token = request.headers.Authorization;
                if ((options.reject && token.indexOf('oc-') >= 0) || (options.rejectPi && token.indexOf('pi-') >= 0)) return {{status:401, headers:{{}}, bodyText:'{{}}'}};
                if (options.retryPi && (token === 'Bearer pi-claude' || token === 'Bearer pi-codex') && (url.endsWith('/usage'))) return {{status:401, headers:{{}}, bodyText:'{{}}'}};
                if (options.retry && token === 'Bearer oc-claude' && url === 'https://api.anthropic.com/api/oauth/usage') {{
                    return {{status:401, headers:{{}}, bodyText:'{{}}'}};
                }}
                if (url === 'https://api.anthropic.com/api/oauth/usage') {{
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify({{
                        five_hour:{{utilization:token === 'Bearer native-claude' ? 11 : token.indexOf('pi-') >= 0 ? 63 : 37, resets_at:'2099-01-01T00:00:00Z'}},
                        cedar_ember:{{}}
                    }})}};
                }}
                if (url === 'https://api.anthropic.com/api/oauth/profile') {{
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify({{organization:{{organization_type:'claude_pro'}}}})}};
                }}
                if (url === 'https://openrouter.ai/api/v1/credits') {{
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify({{data:{{total_credits:10, total_usage:token === 'Bearer native-router' ? 2.5 : token.indexOf('pi-') >= 0 ? 6 : 4}}}})}};
                }}
                if (url === 'https://openrouter.ai/api/v1/key') {{
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify({{data:{{usage_daily:1.25}}}})}};
                }}
                var used = token.indexOf('native-') >= 0 ? 11 : token.indexOf('pi-') >= 0 ? 63 : 37;
                if (url === 'https://chatgpt.com/backend-api/wham/usage') {{
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify({{plan_type:'plus',rate_limit:{{primary_window:{{used_percent:used,limit_window_seconds:18000,reset_at:4070908800}}}},rate_limit_reset_credits:{{available_count:1}}}})}};
                }}
                if (url === 'https://chatgpt.com/backend-api/wham/rate-limit-reset-credits') {{
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify({{rate_limit_reset_credits:{{available_count:1}}}})}};
                }}
                if (url === 'https://api.github.com/copilot_internal/user') {{
                    return {{status:200, headers:{{}}, bodyText:JSON.stringify({{copilot_plan:'pro',quota_reset_date:'2099-01-15T00:00:00Z',quota_snapshots:{{premium_interactions:{{entitlement:100,remaining:100-used,percent_remaining:100-used}}}}}})}};
                }}
                observed.violations.push(request); persist();
                throw new Error('unexpected HTTP fixture request');
            }};
            persist();
        }})();
        "#,
        options = options,
        path = json!(observation.to_string_lossy()),
    );
    plugin.entry_script = harness + plugin.entry_script.as_str();
    plugin
}

pub fn observed(paths: &HermeticPaths) -> Value {
    let observed: Value = serde_json::from_str(
        &fs::read_to_string(paths.root.join("observed.json")).expect("fixture initialized"),
    )
    .unwrap();
    assert_eq!(observed["violations"], json!([]), "{observed}");
    assert!(
        observed["subscriptions"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("~/{AUTH}")))
    );
    observed
}

pub fn probe(paths: &HermeticPaths, id: &str, options: Value) -> (Vec<Value>, Value) {
    let plugin = install(paths, id, options);
    let result = runtime::run_probe_in_sandbox(
        &plugin,
        &paths.app_data,
        "test",
        Some(&paths.overrides),
        &paths.root,
    );
    assert!(
        result
            .subscriptions
            .iter()
            .any(|s| s.path == paths.root.join(AUTH)),
        "missing OpenCode subscription: {:?}",
        result.outputs.iter().map(|o| &o.lines).collect::<Vec<_>>()
    );
    let outputs = result
        .outputs
        .iter()
        .map(|o| serde_json::to_value(o).unwrap())
        .collect();
    let mut observation = observed(paths);
    observation["collectedSubscriptions"] = json!(result.subscriptions.iter().map(|subscription| json!({"path":subscription.path,"existed":subscription.existed_at_declaration})).collect::<Vec<_>>());
    (outputs, observation)
}

pub fn pi_auth() -> Value {
    json!({
        "anthropic": {"type":"oauth", "access":"pi-claude", "refresh":"pi-claude-refresh", "expires":4102444800000_u64},
        "openai-codex": {"type":"oauth", "access":"pi-codex", "refresh":"pi-codex-refresh", "expires":4102444800000_u64, "accountId":"pi-account"},
        "github-copilot": {"type":"oauth", "access":"pi-inference", "refresh":"pi-copilot", "expires":0},
        "openrouter": {"type":"api_key", "key":"pi-router"},
        "unrelated": {"type":"api_key", "key":"untouched"}
    })
}

pub fn native(paths: &HermeticPaths) {
    write(paths, ".claude/.credentials.json", &json!({"claudeAiOauth":{
        "accessToken":"native-claude", "refreshToken":"native-refresh", "expiresAt":4102444800000_u64
    }}).to_string());
    write(
        paths,
        ".config/openusage/openrouter.json",
        r#"{"apiKey":"native-router"}"#,
    );
    write(paths, ".codex/auth.json", &json!({"tokens":{
        "access_token":"native-codex", "refresh_token":"native-codex-refresh", "account_id":"native-account"
    }}).to_string());
    let directory = paths.app_data.join("plugins_data/copilot");
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("auth.json"), r#"{"token":"native-copilot"}"#).unwrap();
}
