use openusage_cli::plugin_engine::script_patch;
use rquickjs::{Context, Runtime};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn codex_plugin_script() -> String {
    let path = repo_root().join("vendor/openusage/plugins/codex/plugin.js");
    fs::read_to_string(path).expect("read codex plugin")
}

fn codex_override_script() -> String {
    let path = repo_root().join("plugin-overrides/codex.js");
    fs::read_to_string(path).expect("read codex override")
}

fn execute_probe_with_setup(setup_script: &str) -> Value {
    let plugin_script = codex_plugin_script();
    let override_script = codex_override_script();
    let transformed =
        script_patch::transform_plugin_script("codex", &plugin_script, Some(&override_script))
            .expect("transform codex plugin for test");

    let rt = Runtime::new().expect("runtime");
    let ctx = Context::full(&rt).expect("context");

    ctx.with(|ctx| {
        ctx.eval::<(), _>(HARNESS_SCRIPT.as_bytes())
            .expect("eval harness");
        ctx.eval::<(), _>(setup_script.as_bytes())
            .expect("eval setup");
        ctx.eval::<(), _>(transformed.script.as_bytes())
            .expect("eval plugin");
        ctx.eval::<(), _>(override_script.as_bytes())
            .expect("eval override script");

        let json: String = ctx
            .eval(PROBE_EXEC_SCRIPT.as_bytes())
            .expect("execute probe script");
        serde_json::from_str(&json).expect("parse probe output")
    })
}

#[test]
fn codex_override_uses_opencode_fallback_auth_when_primary_auth_missing() {
    let output = execute_probe_with_setup(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access",
            expires: 1776806966592,
            accountId: "fallback-account"
          }
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
    );

    assert_eq!(output["ok"], Value::Bool(true));

    let first_request = output["state"]["requests"]
        .as_array()
        .and_then(|arr| arr.first())
        .expect("first request");

    assert_eq!(
        first_request["authorization"],
        Value::String("Bearer fallback-access".to_string())
    );
    assert_eq!(
        first_request["accountId"],
        Value::String("fallback-account".to_string())
    );
}

#[test]
fn codex_override_preserves_original_auth_path_priority() {
    let output = execute_probe_with_setup(
        r#"
        __test_state.files["~/.config/codex/auth.json"] = JSON.stringify({
          tokens: {
            access_token: "primary-access"
          },
          last_refresh: "2026-04-19T00:00:00.000Z"
        });
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access",
            expires: 1776806966592,
            accountId: "fallback-account"
          }
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
    );

    assert_eq!(output["ok"], Value::Bool(true));

    let first_request = output["state"]["requests"]
        .as_array()
        .and_then(|arr| arr.first())
        .expect("first request");

    assert_eq!(
        first_request["authorization"],
        Value::String("Bearer primary-access".to_string())
    );
}

#[test]
fn codex_override_persists_refresh_back_to_opencode_auth_file() {
    let output = execute_probe_with_setup(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "old-refresh",
            access: "old-access",
            expires: 1776806966592,
            accountId: "acc-123"
          }
        });
        __test_state.responses.usage.push({
          status: 401,
          headers: {},
          bodyText: JSON.stringify({ error: "expired" })
        });
        __test_state.responses.refresh.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({
            access_token: "new-access",
            refresh_token: "new-refresh"
          })
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
    );

    assert_eq!(output["ok"], Value::Bool(true));

    let requests = output["state"]["requests"]
        .as_array()
        .expect("requests array");
    assert!(
        requests.iter().any(|req| req["url"]
            .as_str()
            .unwrap_or_default()
            .contains("oauth/token")),
        "refresh request should be executed"
    );

    let usage_requests: Vec<&Value> = requests
        .iter()
        .filter(|req| {
            req["url"]
                .as_str()
                .unwrap_or_default()
                .contains("/wham/usage")
        })
        .collect();
    assert!(usage_requests.len() >= 2, "expected two usage requests");
    assert_eq!(
        usage_requests[1]["authorization"],
        Value::String("Bearer new-access".to_string())
    );

    let updated_text = output["state"]["files"]["~/.local/share/opencode/auth.json"]
        .as_str()
        .expect("updated opencode auth file text");
    let updated: Value = serde_json::from_str(updated_text).expect("updated auth json");
    assert_eq!(
        updated["openai"]["access"],
        Value::String("new-access".to_string())
    );
    assert_eq!(
        updated["openai"]["refresh"],
        Value::String("new-refresh".to_string())
    );
    assert_eq!(
        updated["openai"]["accountId"],
        Value::String("acc-123".to_string())
    );
}

#[test]
fn codex_override_preserves_other_providers_when_persisting_refresh() {
    let output = execute_probe_with_setup(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "old-refresh",
            access: "old-access",
            expires: 1776806966592,
            accountId: "acc-123"
          },
          google: {
            type: "oauth",
            refresh: "google-refresh",
            access: "google-access",
            expires: 1775261213360
          }
        });
        __test_state.responses.usage.push({
          status: 401,
          headers: {},
          bodyText: JSON.stringify({ error: "expired" })
        });
        __test_state.responses.refresh.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({
            access_token: "new-access",
            refresh_token: "new-refresh"
          })
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
    );

    assert_eq!(output["ok"], Value::Bool(true));

    let updated_text = output["state"]["files"]["~/.local/share/opencode/auth.json"]
        .as_str()
        .expect("updated opencode auth file text");
    let updated: Value = serde_json::from_str(updated_text).expect("updated auth json");

    assert_eq!(
        updated["openai"]["access"],
        Value::String("new-access".to_string())
    );
    assert_eq!(
        updated["google"]["access"],
        Value::String("google-access".to_string())
    );
    assert_eq!(
        updated["google"]["refresh"],
        Value::String("google-refresh".to_string())
    );
}

#[test]
fn codex_override_tries_multiple_opencode_auth_paths() {
    let output = execute_probe_with_setup(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          google: {
            type: "oauth",
            refresh: "g-refresh",
            access: "g-access",
            expires: 1775261213360
          }
        });
        __test_state.files["~/.config/opencode/auth.json"] = JSON.stringify({
          openai: {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access-2",
            expires: 1776806966592,
            accountId: "fallback-account-2"
          }
        });
        __test_state.responses.usage.push({
          status: 200,
          headers: {},
          bodyText: JSON.stringify({})
        });
        "#,
    );

    assert_eq!(output["ok"], Value::Bool(true));

    let first_request = output["state"]["requests"]
        .as_array()
        .and_then(|arr| arr.first())
        .expect("first request");

    assert_eq!(
        first_request["authorization"],
        Value::String("Bearer fallback-access-2".to_string())
    );
    assert_eq!(
        first_request["accountId"],
        Value::String("fallback-account-2".to_string())
    );
}

#[test]
fn codex_override_keeps_not_logged_in_error_without_valid_fallback_payload() {
    let output = execute_probe_with_setup(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = "{bad-json";
        "#,
    );

    assert_eq!(output["ok"], Value::Bool(false));
    let error = output["error"].as_str().unwrap_or_default();
    assert!(error.contains("Not logged in"));
}

const HARNESS_SCRIPT: &str = r#"
(function () {
  globalThis.__test_state = {
    env: {},
    files: {},
    requests: [],
    logs: [],
    responses: {
      usage: [],
      refresh: []
    },
    ccusageResult: { status: "no_runner" }
  };

  function cloneHeaders(input) {
    var out = {};
    if (!input || typeof input !== "object") return out;
    var keys = Object.keys(input);
    for (var i = 0; i < keys.length; i++) {
      out[keys[i]] = input[keys[i]];
    }
    return out;
  }

  var ctx = {
    host: {
      fs: {
        exists: function (path) {
          return Object.prototype.hasOwnProperty.call(globalThis.__test_state.files, path);
        },
        readText: function (path) {
          if (!Object.prototype.hasOwnProperty.call(globalThis.__test_state.files, path)) {
            throw new Error("file not found: " + path);
          }
          return globalThis.__test_state.files[path];
        },
        writeText: function (path, text) {
          globalThis.__test_state.files[path] = String(text);
        },
        listDir: function () { return []; }
      },
      env: {
        get: function (name) {
          if (Object.prototype.hasOwnProperty.call(globalThis.__test_state.env, name)) {
            return globalThis.__test_state.env[name];
          }
          return null;
        }
      },
      keychain: {
        readGenericPassword: function () { return null; },
        writeGenericPassword: function () {},
        writeGenericPasswordForCurrentUser: function () {},
        readGenericPasswordForCurrentUser: function () { return null; },
        deleteGenericPassword: function () {}
      },
      http: {
        request: function (opts) {
          opts = opts || {};
          var headers = cloneHeaders(opts.headers);
          var url = String(opts.url || "");
          globalThis.__test_state.requests.push({
            method: String(opts.method || "GET"),
            url: url,
            authorization: headers.Authorization || null,
            accountId: headers["ChatGPT-Account-Id"] || null
          });

          var pool = url.indexOf("/oauth/token") !== -1
            ? globalThis.__test_state.responses.refresh
            : globalThis.__test_state.responses.usage;
          var resp = pool.length > 0 ? pool.shift() : null;
          if (!resp) {
            return { status: 500, headers: {}, bodyText: "{}" };
          }

          var bodyText = typeof resp.bodyText === "string"
            ? resp.bodyText
            : JSON.stringify(resp.bodyText || {});

          return {
            status: Number(resp.status || 200),
            headers: cloneHeaders(resp.headers),
            bodyText: bodyText
          };
        }
      },
      ccusage: {
        query: function () {
          return globalThis.__test_state.ccusageResult;
        }
      },
      log: {
        debug: function (msg) { globalThis.__test_state.logs.push("debug:" + String(msg)); },
        info: function (msg) { globalThis.__test_state.logs.push("info:" + String(msg)); },
        warn: function (msg) { globalThis.__test_state.logs.push("warn:" + String(msg)); },
        error: function (msg) { globalThis.__test_state.logs.push("error:" + String(msg)); }
      }
    },
    util: {
      tryParseJson: function (text) {
        if (text === null || text === undefined) return null;
        try {
          return JSON.parse(String(text));
        } catch (_) {
          return null;
        }
      },
      parseDateMs: function (value) {
        if (typeof value === "number" && Number.isFinite(value)) return value;
        if (typeof value === "string") {
          var parsed = Date.parse(value);
          return Number.isFinite(parsed) ? parsed : null;
        }
        return null;
      },
      request: function (opts) {
        return ctx.host.http.request(opts);
      },
      isAuthStatus: function (status) {
        return status === 401 || status === 403;
      },
      retryOnceOnAuth: function (opts) {
        var first = opts.request();
        if (!ctx.util.isAuthStatus(first.status)) return first;
        var refreshed = opts.refresh();
        if (!refreshed) return first;
        return opts.request(refreshed);
      },
      toIso: function (value) {
        if (value === null || value === undefined) return null;
        if (typeof value === "number" && Number.isFinite(value)) {
          var ms = Math.abs(value) < 1e10 ? value * 1000 : value;
          var dNum = new Date(ms);
          return Number.isFinite(dNum.getTime()) ? dNum.toISOString() : null;
        }
        if (typeof value === "string") {
          var dStr = new Date(value);
          return Number.isFinite(dStr.getTime()) ? dStr.toISOString() : null;
        }
        return null;
      }
    },
    line: {
      text: function (opts) { return { type: "text", label: opts.label, value: opts.value }; },
      progress: function (opts) {
        return {
          type: "progress",
          label: opts.label,
          used: opts.used,
          limit: opts.limit,
          format: opts.format,
          resetsAt: opts.resetsAt,
          periodDurationMs: opts.periodDurationMs
        };
      },
      badge: function (opts) { return { type: "badge", label: opts.label, text: opts.text }; }
    },
    fmt: {
      planLabel: function (value) { return String(value || ""); }
    }
  };

  globalThis.__test_ctx = ctx;
})();
"#;

const PROBE_EXEC_SCRIPT: &str = r#"
(function () {
  try {
    var result = globalThis.__openusage_plugin.probe(globalThis.__test_ctx);
    return JSON.stringify({
      ok: true,
      result: result,
      state: {
        files: globalThis.__test_state.files,
        requests: globalThis.__test_state.requests,
        logs: globalThis.__test_state.logs
      }
    });
  } catch (e) {
    return JSON.stringify({
      ok: false,
      error: String(e),
      state: {
        files: globalThis.__test_state.files,
        requests: globalThis.__test_state.requests,
        logs: globalThis.__test_state.logs
      }
    });
  }
})();
"#;
