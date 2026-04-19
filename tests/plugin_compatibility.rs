use openusage_cli::plugin_engine::manifest;
use openusage_cli::plugin_engine::runtime;
use rquickjs::{Context, Ctx, Function, Object, Runtime, Value};
use std::path::PathBuf;

fn vendor_plugins_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/openusage/plugins")
}

#[test]
fn vendor_plugins_are_loadable() {
    let plugins = manifest::load_plugins_from_dir(&vendor_plugins_dir());
    assert!(
        plugins.len() >= 10,
        "expected openusage plugins to be vendored"
    );
    assert!(plugins.iter().any(|p| p.manifest.id == "claude"));
    assert!(plugins.iter().any(|p| p.manifest.id == "codex"));
    assert!(plugins.iter().any(|p| p.manifest.id == "cursor"));
}

#[test]
fn mock_plugin_runs_in_real_runtime() {
    let plugins = manifest::load_plugins_from_dir(&vendor_plugins_dir());
    let plugin = plugins
        .into_iter()
        .find(|p| p.manifest.id == "mock")
        .expect("mock plugin not found");

    let tmp = tempfile::tempdir().expect("temp dir");
    let output = runtime::run_probe(&plugin, tmp.path(), "0.1.0-test", None);
    assert_eq!(output.provider_id, "mock");
    assert!(output.lines.len() > 5);
}

#[test]
fn all_vendored_plugins_register_and_probe_with_stub_ctx() {
    let plugins = manifest::load_plugins_from_dir(&vendor_plugins_dir());
    for plugin in plugins {
        let err = run_probe_with_stub_ctx(&plugin.entry_script)
            .unwrap_or_else(|e| panic!("plugin {} failed to evaluate: {}", plugin.manifest.id, e));

        if let Some(message) = err {
            let lower = message.to_lowercase();
            assert!(
                !lower.contains("not a function")
                    && !lower.contains("cannot read")
                    && !lower.contains("undefined")
                    && !lower.contains("typeerror"),
                "plugin {} failed due to compatibility issue: {}",
                plugin.manifest.id,
                message
            );
        }
    }
}

fn run_probe_with_stub_ctx(entry_script: &str) -> Result<Option<String>, String> {
    let rt = Runtime::new().map_err(|e| e.to_string())?;
    let ctx = Context::full(&rt).map_err(|e| e.to_string())?;

    ctx.with(|ctx| {
        ctx.eval::<(), _>(STUB_CTX_SCRIPT.as_bytes())
            .map_err(|_| extract_error_string(&ctx))?;
        ctx.eval::<(), _>(entry_script.as_bytes())
            .map_err(|_| extract_error_string(&ctx))?;

        let globals = ctx.globals();
        let plugin_obj: Object = globals
            .get("__openusage_plugin")
            .map_err(|_| "missing __openusage_plugin".to_string())?;
        let probe_fn: Function = plugin_obj
            .get("probe")
            .map_err(|_| "missing probe()".to_string())?;
        let probe_ctx: Value = globals
            .get("__compat_ctx")
            .map_err(|_| "missing __compat_ctx".to_string())?;

        let _registered_id: String = plugin_obj
            .get("id")
            .map_err(|_| "plugin did not export id".to_string())?;

        match probe_fn.call::<_, Value>((probe_ctx,)) {
            Ok(_) => Ok(None),
            Err(_) => Ok(Some(extract_error_string(&ctx))),
        }
    })
}

fn extract_error_string(ctx: &Ctx<'_>) -> String {
    let exc = ctx.catch();
    if exc.is_null() || exc.is_undefined() {
        return "unknown quickjs error".to_string();
    }
    if let Some(str_val) = exc.as_string() {
        let message: String = str_val.to_string().unwrap_or_default();
        if !message.trim().is_empty() {
            return message;
        }
    }
    "unknown quickjs error".to_string()
}

const STUB_CTX_SCRIPT: &str = r#"
(function () {
  var ctx = {
    nowIso: "2026-02-02T00:00:00.000Z",
    app: {
      version: "0.0.0",
      platform: "linux",
      appDataDir: "/tmp/openusage-compat",
      pluginDataDir: "/tmp/openusage-compat/plugin"
    },
    host: {
      fs: {
        exists: function () { return false; },
        readText: function () { throw "not found"; },
        writeText: function () {},
        listDir: function () { return []; }
      },
      env: {
        get: function () { return null; }
      },
      keychain: {
        readGenericPassword: function () { throw "not found"; },
        readGenericPasswordForCurrentUser: function () { throw "not found"; },
        writeGenericPassword: function () {},
        writeGenericPasswordForCurrentUser: function () {},
        deleteGenericPassword: function () {}
      },
      crypto: {
        decryptAes256Gcm: function () { throw "unsupported in stub"; },
        encryptAes256Gcm: function () { throw "unsupported in stub"; }
      },
      sqlite: {
        query: function () { return "[]"; },
        exec: function () {}
      },
      http: {
        request: function () { return { status: 401, headers: {}, bodyText: "{}" }; }
      },
      ls: {
        discover: function () { return null; }
      },
      ccusage: {
        query: function () { return { status: "no_runner" }; }
      },
      log: {
        trace: function () {},
        debug: function () {},
        info: function () {},
        warn: function () {},
        error: function () {}
      }
    }
  };

  ctx.line = {
    text: function (opts) {
      var line = { type: "text", label: opts.label, value: opts.value };
      if (opts.color) line.color = opts.color;
      if (opts.subtitle) line.subtitle = opts.subtitle;
      return line;
    },
    progress: function (opts) {
      var line = { type: "progress", label: opts.label, used: opts.used, limit: opts.limit, format: opts.format };
      if (opts.resetsAt) line.resetsAt = opts.resetsAt;
      if (opts.periodDurationMs) line.periodDurationMs = opts.periodDurationMs;
      if (opts.color) line.color = opts.color;
      return line;
    },
    badge: function (opts) {
      var line = { type: "badge", label: opts.label, text: opts.text };
      if (opts.color) line.color = opts.color;
      if (opts.subtitle) line.subtitle = opts.subtitle;
      return line;
    }
  };

  ctx.fmt = {
    planLabel: function (value) {
      var text = String(value || "").trim();
      if (!text) return "";
      return text.replace(/(^|\s)([a-z])/g, function (match, space, letter) {
        return space + letter.toUpperCase();
      });
    },
    resetIn: function (secondsUntil) {
      if (!Number.isFinite(secondsUntil) || secondsUntil < 0) return null;
      var totalMinutes = Math.floor(secondsUntil / 60);
      var totalHours = Math.floor(totalMinutes / 60);
      var days = Math.floor(totalHours / 24);
      var hours = totalHours % 24;
      var minutes = totalMinutes % 60;
      if (days > 0) return days + "d " + hours + "h";
      if (totalHours > 0) return totalHours + "h " + minutes + "m";
      if (totalMinutes > 0) return totalMinutes + "m";
      return "<1m";
    },
    dollars: function (cents) {
      return Math.round((cents / 100) * 100) / 100;
    },
    date: function (unixMs) {
      var d = new Date(Number(unixMs));
      var months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
      return months[d.getMonth()] + " " + String(d.getDate());
    }
  };

  var b64chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  ctx.base64 = {
    decode: function (str) {
      str = str.replace(/-/g, "+").replace(/_/g, "/");
      while (str.length % 4) str += "=";
      str = str.replace(/=+$/, "");
      var result = "";
      var len = str.length;
      var i = 0;
      while (i < len) {
        var remaining = len - i;
        var a = b64chars.indexOf(str.charAt(i++));
        var b = b64chars.indexOf(str.charAt(i++));
        var c = remaining > 2 ? b64chars.indexOf(str.charAt(i++)) : 0;
        var d = remaining > 3 ? b64chars.indexOf(str.charAt(i++)) : 0;
        var n = (a << 18) | (b << 12) | (c << 6) | d;
        result += String.fromCharCode((n >> 16) & 0xff);
        if (remaining > 2) result += String.fromCharCode((n >> 8) & 0xff);
        if (remaining > 3) result += String.fromCharCode(n & 0xff);
      }
      return result;
    },
    encode: function (str) {
      var result = "";
      var len = str.length;
      var i = 0;
      while (i < len) {
        var chunkStart = i;
        var a = str.charCodeAt(i++);
        var b = i < len ? str.charCodeAt(i++) : 0;
        var c = i < len ? str.charCodeAt(i++) : 0;
        var bytesInChunk = i - chunkStart;
        var n = (a << 16) | (b << 8) | c;
        result += b64chars.charAt((n >> 18) & 63);
        result += b64chars.charAt((n >> 12) & 63);
        result += bytesInChunk < 2 ? "=" : b64chars.charAt((n >> 6) & 63);
        result += bytesInChunk < 3 ? "=" : b64chars.charAt(n & 63);
      }
      return result;
    }
  };

  ctx.jwt = {
    decodePayload: function (token) {
      try {
        var parts = token.split(".");
        if (parts.length !== 3) return null;
        var decoded = ctx.base64.decode(parts[1]);
        return JSON.parse(decoded);
      } catch (e) {
        return null;
      }
    }
  };

  ctx.util = {
    tryParseJson: function (text) {
      if (text === null || text === undefined) return null;
      var trimmed = String(text).trim();
      if (!trimmed) return null;
      try {
        return JSON.parse(trimmed);
      } catch (e) {
        return null;
      }
    },
    safeJsonParse: function (text) {
      if (text === null || text === undefined) return { ok: false };
      var trimmed = String(text).trim();
      if (!trimmed) return { ok: false };
      try {
        return { ok: true, value: JSON.parse(trimmed) };
      } catch (e) {
        return { ok: false };
      }
    },
    request: function (opts) {
      return ctx.host.http.request(opts);
    },
    requestJson: function (opts) {
      var resp = ctx.util.request(opts);
      var parsed = ctx.util.safeJsonParse(resp.bodyText);
      return { resp: resp, json: parsed.ok ? parsed.value : null };
    },
    isAuthStatus: function (status) {
      return status === 401 || status === 403;
    },
    retryOnceOnAuth: function (opts) {
      var resp = opts.request();
      if (ctx.util.isAuthStatus(resp.status)) {
        var token = opts.refresh();
        if (token) {
          resp = opts.request(token);
        }
      }
      return resp;
    },
    parseDateMs: function (value) {
      if (value instanceof Date) {
        var dateMs = value.getTime();
        return Number.isFinite(dateMs) ? dateMs : null;
      }
      if (typeof value === "number") {
        return Number.isFinite(value) ? value : null;
      }
      if (typeof value === "string") {
        var parsed = Date.parse(value);
        if (Number.isFinite(parsed)) return parsed;
        var n = Number(value);
        return Number.isFinite(n) ? n : null;
      }
      return null;
    },
    toIso: function (value) {
      if (value === null || value === undefined) return null;
      if (typeof value === "string") {
        var s = String(value).trim();
        if (!s) return null;
        var parsed = Date.parse(s);
        if (!Number.isFinite(parsed)) return null;
        return new Date(parsed).toISOString();
      }
      if (typeof value === "number") {
        if (!Number.isFinite(value)) return null;
        var ms = Math.abs(value) < 1e10 ? value * 1000 : value;
        var d = new Date(ms);
        var t = d.getTime();
        if (!Number.isFinite(t)) return null;
        return d.toISOString();
      }
      if (value instanceof Date) {
        var tv = value.getTime();
        if (!Number.isFinite(tv)) return null;
        return value.toISOString();
      }
      return null;
    },
    needsRefreshByExpiry: function (opts) {
      if (!opts) return true;
      if (opts.expiresAtMs === null || opts.expiresAtMs === undefined) return true;
      var nowMs = Number(opts.nowMs);
      var expiresAtMs = Number(opts.expiresAtMs);
      var bufferMs = Number(opts.bufferMs);
      if (!Number.isFinite(nowMs)) return true;
      if (!Number.isFinite(expiresAtMs)) return true;
      if (!Number.isFinite(bufferMs)) bufferMs = 0;
      return nowMs + bufferMs >= expiresAtMs;
    }
  };

  globalThis.__compat_ctx = ctx;
})();
"#;
