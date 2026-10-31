globalThis.__openusage_ast_patch = {
  functions: [
    { target: "loadCredentials", with: "patchLoadCredentials", mode: "wrap" },
    { target: "saveCredentials", with: "patchSaveCredentials", mode: "wrap" },
    { target: "queryTokenUsage", with: "patchQueryTokenUsage", mode: "wrap" },
  ],
};

globalThis.__openusage_function_overrides = {
  functions: [
    { target: "discoverAccounts", with: "patchDiscoverAccounts", mode: "replace" },
  ],
};

var patchLoadCredentials = (function () {
  var authPath = "~/.local/share/opencode/auth.json";
  var credentials = null;
  var selectedEntry = null;
  var credentialError = null;

  function isObject(value) {
    return value !== null && typeof value === "object" && !Array.isArray(value);
  }

  function nonEmpty(value) {
    return typeof value === "string" && value.trim().length > 0;
  }

  function validate(entry) {
    if (!isObject(entry) || entry.type !== "oauth" ||
        !nonEmpty(entry.access) || !nonEmpty(entry.refresh) ||
        typeof entry.expires !== "number" || !Number.isFinite(entry.expires) || entry.expires < 0) {
      throw "Invalid OpenCode anthropic OAuth credentials.";
    }
    return entry;
  }

  function readDocument(ctx) {
    var text;
    try {
      if (!ctx.host.fs.exists(authPath)) return null;
      text = ctx.host.fs.readText(authPath);
    } catch (_) {
      throw "Failed to read OpenCode auth.json.";
    }
    if (!text.trim()) return null;
    var doc;
    try {
      doc = JSON.parse(text);
    } catch (_) {
      throw "Invalid OpenCode auth.json.";
    }
    if (!isObject(doc)) throw "Invalid OpenCode auth.json.";
    return doc;
  }

  globalThis.patchDiscoverAccounts = function (ctx, original) {
    credentials = null;
    selectedEntry = null;
    credentialError = null;
    var accounts = original ? original(ctx) : [
      { id: "default", errorPolicy: "hide-if-other-account" },
    ];
    try {
      var doc = readDocument(ctx);
      if (!doc || doc.anthropic === undefined) return accounts;
      var entry = validate(doc.anthropic);
      selectedEntry = { access: entry.access, refresh: entry.refresh, expires: entry.expires };
      credentials = {
        oauth: { accessToken: entry.access.trim(), refreshToken: entry.refresh.trim(), expiresAt: entry.expires },
        source: "opencode",
        fullData: {},
      };
    } catch (error) {
      credentialError = error;
    }
    accounts.push({ id: "opencode-0", origin: "opencode", name: "OpenCode" });
    return accounts;
  };

  globalThis.patchLoadCredentials = function (original, ctx) {
    if (ctx.account.id === "default") return original(ctx);
    if (credentialError) throw credentialError;
    return credentials;
  };

  globalThis.patchSaveCredentials = function (original, ctx, source, serviceName, fullData) {
    if (source !== "opencode") return original(ctx, source, serviceName, fullData);
    // Re-read before writing so other providers and extra entry fields survive refresh.
    var doc = readDocument(ctx);
    if (!doc || doc.anthropic === undefined) throw "OpenCode anthropic credentials disappeared during refresh.";
    validate(doc.anthropic);
    if (doc.anthropic.access !== selectedEntry.access ||
        doc.anthropic.refresh !== selectedEntry.refresh || doc.anthropic.expires !== selectedEntry.expires) {
      throw "OpenCode anthropic credentials changed during refresh.";
    }
    var oauth = fullData.claudeAiOauth;
    doc.anthropic = validate(Object.assign({}, doc.anthropic, {
      access: oauth.accessToken,
      refresh: oauth.refreshToken,
      expires: oauth.expiresAt,
    }));
    try {
      ctx.host.fs.writeText(authPath, JSON.stringify(doc));
      selectedEntry = { access: doc.anthropic.access, refresh: doc.anthropic.refresh, expires: doc.anthropic.expires };
    } catch (_) {
      // Native refresh propagates string failures, but swallows Error objects.
      throw "Failed to save refreshed OpenCode anthropic credentials.";
    }
  };

  globalThis.patchQueryTokenUsage = function (original, ctx, homePath) {
    // Claude Code logs cannot be attributed to the separate OpenCode login.
    if (ctx.account.id !== "default") return { status: "no_runner", data: null };
    return original(ctx, homePath);
  };

  globalThis.__openusage_ctx.host.fs.subscribeFile(authPath);
  return globalThis.patchLoadCredentials;
})();

var patchSaveCredentials = (function () { return globalThis.patchSaveCredentials; })();
var patchQueryTokenUsage = (function () { return globalThis.patchQueryTokenUsage; })();
