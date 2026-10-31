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
  var authPath = "~/.local/share/opencode/auth.json";
  var apiKey = null;
  var credentialError = null;

  function isObject(value) {
    return value !== null && typeof value === "object" && !Array.isArray(value);
  }

  function readEntry(ctx) {
    var text;
    try {
      if (!ctx.host.fs.exists(authPath)) return undefined;
      text = ctx.host.fs.readText(authPath);
    } catch (_) {
      throw "Failed to read OpenCode auth.json.";
    }
    if (!text.trim()) return undefined;
    var doc;
    try {
      doc = JSON.parse(text);
    } catch (_) {
      throw "Invalid OpenCode auth.json.";
    }
    if (!isObject(doc)) throw "Invalid OpenCode auth.json.";
    return doc.openrouter;
  }

  globalThis.patchDiscoverAccounts = function (ctx, original) {
    apiKey = null;
    credentialError = null;
    var accounts = original ? original(ctx) : [
      { id: "default", errorPolicy: "hide-if-other-account" },
    ];
    try {
      var entry = readEntry(ctx);
      if (entry === undefined) return accounts;
      if (!isObject(entry) || entry.type !== "api" ||
          typeof entry.key !== "string" || !entry.key.trim()) {
        throw "Invalid OpenCode openrouter API credentials.";
      }
      apiKey = entry.key.trim();
    } catch (error) {
      credentialError = error;
    }
    accounts.push({ id: "opencode-0", origin: "opencode", name: "OpenCode" });
    return accounts;
  };

  globalThis.patchLoadApiKey = function (original, ctx) {
    if (ctx.account.id === "default") return original(ctx);
    if (credentialError) throw credentialError;
    return apiKey;
  };

  globalThis.__openusage_ctx.host.fs.subscribeFile(authPath);
  return globalThis.patchLoadApiKey;
})();
