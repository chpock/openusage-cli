globalThis.__openusage_ast_patch = {
  functions: [
    { target: "loadApiKey", with: "patchLoadApiKey", mode: "wrap" },
    { target: "creditsLines", with: "patchCreditsLines", mode: "wrap" },
    { target: "appendSpend", with: "patchAppendSpend", mode: "wrap" },
  ],
};

var patchCreditsLines = (function () {
  function readNumber(value) {
    if (typeof value === "number") return Number.isFinite(value) ? value : null;
    if (typeof value !== "string" || !value.trim()) return null;
    var number = Number(value);
    return Number.isFinite(number) ? number : null;
  }

  function usdText(ctx, amount) {
    var cents = amount * 100;
    // Numbers this large cannot carry fractional-dollar precision.
    return "$" + String(Number.isFinite(cents) ? ctx.fmt.dollars(cents) : amount);
  }

  globalThis.patchAppendSpend = function (original, ctx, value, label, lines) {
    var before = lines.length;
    original(ctx, value, label, lines);
    if (lines.length > before) {
      lines[before].value = usdText(ctx, Math.max(0, readNumber(value)));
    }
  };

  return function (original, ctx, data) {
    var lines = original(ctx, data);
    for (var i = 0; i < lines.length; i += 1) {
      if (lines[i].type === "text" && lines[i].label === "Balance") {
        var used = Math.max(0, readNumber(data.total_usage));
        var credits = Math.max(0, readNumber(data.total_credits) || 0);
        lines[i].value = usdText(ctx, Math.max(0, credits - used));
      }
    }
    return lines;
  };
})();

var patchAppendSpend = (function () {
  return globalThis.patchAppendSpend;
})();


globalThis.__openusage_function_overrides = {
  functions: [
    { target: "discoverAccounts", with: "patchDiscoverAccounts", mode: "replace" },
  ],
};

var patchLoadApiKey = (function () {
  var sources = [
    { id: "opencode-0", origin: "opencode", name: "OpenCode", path: "~/.local/share/opencode/auth.json" },
    { id: "pi-0", origin: "pi", name: "Pi", path: "~/.pi/agent/auth.json" },
  ];
  var routes = {};

  function isObject(value) {
    return value !== null && typeof value === "object" && !Array.isArray(value);
  }

  function readEntry(ctx, source) {
    var text;
    try {
      if (!ctx.host.fs.exists(source.path)) return undefined;
      text = source.origin === "pi" ? ctx.host.fs.withAuthFileLock(source.path, function () {
        return ctx.host.fs.readText(source.path);
      }) : ctx.host.fs.readText(source.path);
    } catch (_) {
      throw "Failed to read " + source.name + " auth.json.";
    }
    if (!text.trim()) return undefined;
    var doc;
    try {
      doc = JSON.parse(text);
    } catch (_) {
      throw "Invalid " + source.name + " auth.json.";
    }
    if (!isObject(doc)) throw "Invalid " + source.name + " auth.json.";
    return doc.openrouter;
  }

  function readKey(entry, source) {
    if (!isObject(entry)) throw "Invalid " + source.name + " openrouter API credentials.";
    var key;
    if (source.origin === "opencode" && entry.type === "api") key = entry.key;
    if (source.origin === "pi") {
      if (entry.type === "api_key") {
        key = entry.key;
        if (typeof key === "string" && (key.trim().charAt(0) === "!" || key.indexOf("$") !== -1)) {
          throw "Pi OpenRouter API-key expressions are not supported.";
        }
      } else if (entry.type === "oauth" && entry.refresh === "" &&
          typeof entry.expires === "number" && Number.isFinite(entry.expires) && entry.expires >= 0) {
        key = entry.access;
      }
    }
    if (typeof key !== "string" || !key.trim()) throw "Invalid " + source.name + " openrouter API credentials.";
    return key.trim();
  }

  globalThis.patchDiscoverAccounts = function (ctx, original) {
    routes = {};
    var accounts = original ? original(ctx) : [
      { id: "default", errorPolicy: "hide-if-other-account" },
    ];
    for (var i = 0; i < sources.length; i++) {
      var source = sources[i];
      var route = {};
      try {
        var entry = readEntry(ctx, source);
        if (entry === undefined) continue;
        route.key = readKey(entry, source);
      } catch (error) {
        route.error = error;
      }
      routes[source.id] = route;
      accounts.push({ id: source.id, origin: source.origin, name: source.name });
    }
    return accounts;
  };

  globalThis.patchLoadApiKey = function (original, ctx) {
    if (ctx.account.id === "default") return original(ctx);
    var route = routes[ctx.account.id];
    if (route.error) throw route.error;
    return route.key;
  };

  for (var i = 0; i < sources.length; i++) {
    globalThis.__openusage_ctx.host.fs.subscribeFile(sources[i].path);
  }
  return globalThis.patchLoadApiKey;
})();
