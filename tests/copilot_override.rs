use openusage_cli::plugin_engine::script_patch;
use rquickjs::{Context, Runtime};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn copilot_plugin_script() -> String {
    let path = repo_root().join("vendor/openusage/plugins/copilot/plugin.js");
    fs::read_to_string(path).expect("read copilot plugin")
}

fn copilot_override_script() -> String {
    let path = repo_root().join("plugin-overrides/copilot.js");
    fs::read_to_string(path).expect("read copilot override")
}

fn execute_probe_with_setup(setup_script: &str) -> Value {
    let plugin_script = copilot_plugin_script();
    let override_script = copilot_override_script();
    let transformed =
        script_patch::transform_plugin_script("copilot", &plugin_script, Some(&override_script))
            .expect("transform copilot plugin for test");

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
fn copilot_override_uses_opencode_fallback_auth_when_primary_auth_missing() {
    let output = execute_probe_with_setup(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          "github-copilot": {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access",
            expires: 0
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
        Value::String("token fallback-access".to_string())
    );
}

#[test]
fn copilot_override_preserves_original_auth_priority() {
    let output = execute_probe_with_setup(
        r#"
        __test_state.keychain["OpenUsage-copilot"] = JSON.stringify({
          token: "primary-token"
        });
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          "github-copilot": {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access",
            expires: 0
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
        Value::String("token primary-token".to_string())
    );
}

#[test]
fn copilot_override_tries_multiple_opencode_auth_paths() {
    let output = execute_probe_with_setup(
        r#"
        __test_state.files["~/.local/share/opencode/auth.json"] = JSON.stringify({
          google: {
            type: "oauth",
            refresh: "google-refresh",
            access: "google-access",
            expires: 1
          }
        });
        __test_state.files["~/.config/opencode/auth.json"] = JSON.stringify({
          "github-copilot": {
            type: "oauth",
            refresh: "fallback-refresh",
            access: "fallback-access-2",
            expires: 0
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
        Value::String("token fallback-access-2".to_string())
    );
}

#[test]
fn copilot_override_keeps_not_logged_in_error_without_valid_fallback_payload() {
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
    files: {},
    keychain: {},
    requests: [],
    logs: [],
    responses: {
      usage: []
    }
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

  function tryParseJson(text) {
    if (text === null || text === undefined) return null;
    var trimmed = String(text).trim();
    if (!trimmed) return null;
    try {
      return JSON.parse(trimmed);
    } catch (_) {
      return null;
    }
  }

  var ctx = {
    app: {
      pluginDataDir: "~/.config/openusage/plugins/copilot"
    },
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
      keychain: {
        readGenericPassword: function (service) {
          if (Object.prototype.hasOwnProperty.call(globalThis.__test_state.keychain, service)) {
            return globalThis.__test_state.keychain[service];
          }
          return null;
        },
        writeGenericPassword: function (service, value) {
          globalThis.__test_state.keychain[service] = String(value);
        },
        deleteGenericPassword: function (service) {
          delete globalThis.__test_state.keychain[service];
        }
      },
      http: {
        request: function (opts) {
          opts = opts || {};
          var headers = cloneHeaders(opts.headers);
          var url = String(opts.url || "");
          globalThis.__test_state.requests.push({
            method: String(opts.method || "GET"),
            url: url,
            authorization: headers.Authorization || null
          });

          var resp = globalThis.__test_state.responses.usage.length > 0
            ? globalThis.__test_state.responses.usage.shift()
            : null;
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
      log: {
        info: function (msg) { globalThis.__test_state.logs.push("info:" + String(msg)); },
        warn: function (msg) { globalThis.__test_state.logs.push("warn:" + String(msg)); },
        error: function (msg) { globalThis.__test_state.logs.push("error:" + String(msg)); }
      }
    },
    util: {
      tryParseJson: tryParseJson,
      request: function (opts) {
        return ctx.host.http.request(opts);
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
    base64: {
      decode: function (str) {
        var chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        str = String(str || "").replace(/-/g, "+").replace(/_/g, "/");
        while (str.length % 4) str += "=";
        str = str.replace(/=+$/, "");

        var result = "";
        var i = 0;
        while (i < str.length) {
          var remaining = str.length - i;
          var a = chars.indexOf(str.charAt(i++));
          var b = chars.indexOf(str.charAt(i++));
          var c = remaining > 2 ? chars.indexOf(str.charAt(i++)) : 0;
          var d = remaining > 3 ? chars.indexOf(str.charAt(i++)) : 0;
          var n = (a << 18) | (b << 12) | (c << 6) | d;
          result += String.fromCharCode((n >> 16) & 255);
          if (remaining > 2) result += String.fromCharCode((n >> 8) & 255);
          if (remaining > 3) result += String.fromCharCode(n & 255);
        }
        return result;
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
