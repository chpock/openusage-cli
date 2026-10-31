globalThis.__openusage_ast_patch = {
  functions: [
    { target: "loadCredentials", with: "patchLoadCredentials", mode: "wrap" },
    { target: "saveCredentials", with: "patchSaveCredentials", mode: "wrap" },
    { target: "refreshToken", with: "patchRefreshToken", mode: "wrap" },
    { target: "queryTokenUsage", with: "patchQueryTokenUsage", mode: "wrap" },
  ],
};

globalThis.__openusage_function_overrides = {
  functions: [
    { target: "discoverAccounts", with: "patchDiscoverAccounts", mode: "replace" },
  ],
};

var patchLoadCredentials = (function () {
  var sources = [
    { id: "opencode-0", origin: "opencode", name: "OpenCode", path: "~/.local/share/opencode/auth.json" },
    { id: "pi-0", origin: "pi", name: "Pi", path: "~/.pi/agent/auth.json" },
  ];
  var routes = {};

  function isObject(value) {
    return value !== null && typeof value === "object" && !Array.isArray(value);
  }

  function nonEmpty(value) {
    return typeof value === "string" && value.trim().length > 0;
  }

  function validate(entry, source) {
    if (!isObject(entry) || entry.type !== "oauth" ||
        !nonEmpty(entry.access) || !nonEmpty(entry.refresh) ||
        typeof entry.expires !== "number" || !Number.isFinite(entry.expires) || entry.expires < 0) {
      throw "Invalid " + source.name + " anthropic OAuth credentials.";
    }
    return entry;
  }

  function readDocument(ctx, source, locked) {
    var text;
    try {
      if (!ctx.host.fs.exists(source.path)) return null;
      text = source.origin === "pi" && !locked ? ctx.host.fs.withAuthFileLock(source.path, function () {
        return ctx.host.fs.readText(source.path);
      }) : ctx.host.fs.readText(source.path);
    } catch (_) {
      throw "Failed to read " + source.name + " auth.json.";
    }
    if (!text.trim()) return null;
    var doc;
    try {
      doc = JSON.parse(text);
    } catch (_) {
      throw "Invalid " + source.name + " auth.json.";
    }
    if (!isObject(doc)) throw "Invalid " + source.name + " auth.json.";
    return doc;
  }

  function selectEntry(route, entry) {
    route.selected = { access: entry.access, refresh: entry.refresh, expires: entry.expires };
    route.credentials = {
      oauth: { accessToken: entry.access.trim(), refreshToken: entry.refresh.trim(), expiresAt: entry.expires },
      source: route.source.origin,
      fullData: {},
    };
  }

  globalThis.patchDiscoverAccounts = function (ctx, original) {
    routes = {};
    var accounts = original ? original(ctx) : [
      { id: "default", errorPolicy: "hide-if-other-account" },
    ];
    for (var i = 0; i < sources.length; i++) {
      var source = sources[i];
      var route = { source: source };
      try {
        var doc = readDocument(ctx, source);
        if (!doc || doc.anthropic === undefined) continue;
        selectEntry(route, validate(doc.anthropic, source));
      } catch (error) {
        route.error = error;
      }
      routes[source.id] = route;
      accounts.push({ id: source.id, origin: source.origin, name: source.name });
    }
    return accounts;
  };

  globalThis.patchLoadCredentials = function (original, ctx) {
    if (ctx.account.id === "default") return original(ctx);
    var route = routes[ctx.account.id];
    if (route.error) throw route.error;
    return route.credentials;
  };

  globalThis.patchSaveCredentials = function (original, ctx, source, serviceName, fullData) {
    if (source !== "opencode" && source !== "pi") return original(ctx, source, serviceName, fullData);
    var route = routes[ctx.account.id];
    var selected = route.source;
    var doc = readDocument(ctx, selected, selected.origin === "pi");
    if (!doc || doc.anthropic === undefined) throw selected.name + " anthropic credentials disappeared during refresh.";
    validate(doc.anthropic, selected);
    if (doc.anthropic.access !== route.selected.access ||
        doc.anthropic.refresh !== route.selected.refresh || doc.anthropic.expires !== route.selected.expires) {
      throw selected.name + " anthropic credentials changed during refresh.";
    }
    var oauth = fullData.claudeAiOauth;
    doc.anthropic = validate(Object.assign({}, doc.anthropic, {
      access: oauth.accessToken,
      refresh: oauth.refreshToken,
      // Pi stores a renewal threshold five minutes before the token expires.
      expires: oauth.expiresAt - (selected.origin === "pi" ? 300000 : 0),
    }), selected);
    try {
      ctx.host.fs.writeText(selected.path, JSON.stringify(doc));
      route.selected = { access: doc.anthropic.access, refresh: doc.anthropic.refresh, expires: doc.anthropic.expires };
    } catch (_) {
      // Native refresh propagates string failures, but swallows Error objects.
      throw "Failed to save refreshed " + selected.name + " anthropic credentials.";
    }
  };

  globalThis.patchRefreshToken = function (original, ctx, credentials) {
    var route = routes[ctx.account.id];
    if (!route || route.source.origin !== "pi") return original(ctx, credentials);
    return ctx.host.fs.withAuthFileLock(route.source.path, function () {
      var doc = readDocument(ctx, route.source, true);
      if (!doc || doc.anthropic === undefined) throw "Pi anthropic credentials disappeared during refresh.";
      var entry = validate(doc.anthropic, route.source);
      var changed = entry.access !== route.selected.access || entry.refresh !== route.selected.refresh;
      selectEntry(route, entry);
      Object.assign(credentials.oauth, route.credentials.oauth);
      credentials.fullData = route.credentials.fullData;
      route.credentials = credentials;
      if (changed && entry.expires > Date.now()) return credentials.oauth.accessToken;
      var scoped = Object.create(ctx);
      var util = Object.create(ctx.util);
      util.request = function (request) {
        var response = ctx.util.request(request);
        if (response.status >= 200 && response.status < 300) {
          var body = ctx.util.tryParseJson(response.bodyText);
          if (!body || typeof body.expires_in !== "number" || !Number.isFinite(body.expires_in) || body.expires_in <= 0) {
            throw "Invalid Pi anthropic OAuth refresh expiry.";
          }
        }
        return response;
      };
      Object.defineProperty(scoped, "util", { value: util });
      return original(scoped, credentials);
    });
  };

  globalThis.patchQueryTokenUsage = function (original, ctx, homePath) {
    // Native Claude Code logs cannot be attributed to another credential source.
    if (ctx.account.id !== "default") return { status: "no_runner", data: null };
    return original(ctx, homePath);
  };

  for (var i = 0; i < sources.length; i++) {
    globalThis.__openusage_ctx.host.fs.subscribeFile(sources[i].path);
  }
  return globalThis.patchLoadCredentials;
})();

var patchSaveCredentials = (function () { return globalThis.patchSaveCredentials; })();
var patchRefreshToken = (function () { return globalThis.patchRefreshToken; })();
var patchQueryTokenUsage = (function () { return globalThis.patchQueryTokenUsage; })();
