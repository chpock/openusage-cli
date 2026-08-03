// ── Copilot override: account discovery + source routing ──────────
// This override adds opencode-* accounts to the Copilot plugin.
// It patches loadToken to route through exact selected credential sources
// with no cross-file or native fallback.

globalThis.__openusage_ast_patch = {
  functions: [
    { target: "loadToken", with: "patchLoadToken", mode: "wrap" }
  ]
};

globalThis.__openusage_function_overrides = {
  functions: [
    { target: "discoverAccounts", with: "discoverAccounts", mode: "replace" }
  ]
};

// ── Helpers ───────────────────────────────────────────────────────

function isNonEmptyString(v) {
  return typeof v === "string" && v.length > 0;
}

function isPlainObject(v) {
  return !!v && typeof v === "object" && !Array.isArray(v);
}

function logWarn(ctx, msg) {
  try {
    if (ctx && ctx.host && ctx.host.log && typeof ctx.host.log.warn === "function") {
      ctx.host.log.warn(msg);
    }
  } catch (_) {}
}

function parseJsonLoose(text) {
  if (!isNonEmptyString(text)) return null;
  try {
    return JSON.parse(text);
  } catch (_) {
    return null;
  }
}

function parseCopilotCredential(block) {
  if (!isPlainObject(block)) {
    return { error: "Invalid github-copilot credentials" };
  }
  var token = isNonEmptyString(block.access) ? block.access.trim() : "";
  if (!token) {
    return { error: "Invalid github-copilot credentials" };
  }
  return { token: token };
}

// ── Route registry ────────────────────────────────────────────────
// Populated by discoverAccounts. Each entry maps accountId -> route.

var ROUTE_REGISTRY = {};

var COPILOT_PROVIDER_KEY = "github-copilot";
var OPENCODE_SOURCES = [
  {
    authPath: "~/.local/share/opencode/auth.json",
    accountsPath: "~/.local/share/opencode/accounts.json"
  },
  {
    authPath: "~/.config/opencode/auth.json",
    accountsPath: "~/.config/opencode/accounts.json"
  }
];

function discoverAccounts(ctx, originalDiscoverAccounts) {
  var keys = Object.keys(ROUTE_REGISTRY);
  for (var i = 0; i < keys.length; i++) {
    delete ROUTE_REGISTRY[keys[i]];
  }

  var accounts = [];

  if (typeof originalDiscoverAccounts === "function") {
    try {
      var origAccounts = originalDiscoverAccounts(ctx);
      if (Array.isArray(origAccounts)) {
        for (var oi = 0; oi < origAccounts.length; oi++) {
          accounts.push(origAccounts[oi]);
        }
      }
    } catch (_) {}
  } else {
    accounts.push({
      id: "default",
      errorPolicy: "hide-if-other-account"
    });
  }

  var fs = ctx && ctx.host && ctx.host.fs;
  if (!fs || typeof fs.readText !== "function") {
    return accounts;
  }

  function buildAccountDescriptor(id, label, stableSubjectKey) {
    var d = {
      id: id,
      origin: "opencode",
      isActive: true,
      name: label,
      sourceRef: "opencode-auth:" + id
    };
    if (stableSubjectKey && typeof stableSubjectKey === "string") {
      d.stableSubjectKey = stableSubjectKey;
    }
    return d;
  }

  var opencodeRouteCounter = 0;
  var usedRouteIds = {};

  for (var bi = 0; bi < accounts.length; bi++) {
    if (accounts[bi] && isNonEmptyString(accounts[bi].id)) {
      usedRouteIds[accounts[bi].id] = true;
    }
  }

  function reserveRouteId(preferred) {
    if (isNonEmptyString(preferred) && !usedRouteIds[preferred]) {
      usedRouteIds[preferred] = true;
      return preferred;
    }
    return nextRouteId();
  }

  function nextRouteId() {
    var id;
    do {
      id = "opencode-" + String(opencodeRouteCounter);
      opencodeRouteCounter += 1;
    } while (usedRouteIds[id]);
    usedRouteIds[id] = true;
    return id;
  }

  function pushErrorRoute(routeId, path, message, label) {
    ROUTE_REGISTRY[routeId] = {
      path: path,
      token: null,
      error: message
    };
    accounts.push(buildAccountDescriptor(routeId, label || routeId));
    logWarn(ctx, "copilot override: " + message + " (path: " + path + ", account: " + routeId + ")");
  }

  function readJsonFile(path, missingState, kindLabel) {
    var fileExists;
    try {
      if (typeof fs.exists === "function") {
        fileExists = fs.exists(path);
      } else {
        fileExists = null;
      }
    } catch (e) {
      return { kind: "error", error: "Failed to check " + kindLabel + " file: " + String(e) };
    }

    if (fileExists === false) {
      return { kind: missingState };
    }

    var text;
    try {
      text = fs.readText(path);
    } catch (e) {
      if (fileExists === null && String(e).toLowerCase().indexOf("file not found") !== -1) {
        return { kind: missingState };
      }
      return { kind: "error", error: "Failed to read " + kindLabel + " file: " + String(e) };
    }

    if (!isNonEmptyString(text)) {
      return { kind: "empty" };
    }

    var doc = parseJsonLoose(text);
    if (!isPlainObject(doc)) {
      return { kind: "error", error: "Invalid " + kindLabel + " file" };
    }

    return { kind: "value", doc: doc };
  }

  function readAuthToken(authPath) {
    var authResult = readJsonFile(authPath, "missing", "auth");
    if (authResult.kind !== "value") {
      if (authResult.kind === "missing" || authResult.kind === "empty") {
        return { error: "Invalid github-copilot credentials" };
      }
      return { error: authResult.error };
    }

    var copilot = authResult.doc[COPILOT_PROVIDER_KEY];
    if (copilot === undefined) {
      return { error: "Invalid github-copilot credentials" };
    }

    return parseCopilotCredential(copilot);
  }

  function validateAccountsEntries(entries) {
    var activeCount = 0;
    var seenAccountIds = {};

    for (var vi = 0; vi < entries.length; vi++) {
      var entry = entries[vi];
      if (!isPlainObject(entry)) {
        continue;
      }

      if (entry.isActive === true) {
        activeCount += 1;
      }

      var aid = isNonEmptyString(entry.accountId) ? entry.accountId.trim() : "";
      if (!aid) {
        continue;
      }
      if (seenAccountIds[aid]) {
        return "Accounts configuration error: duplicate account name '" + aid + "'. Each accountId must be unique.";
      }
      seenAccountIds[aid] = true;
    }

    if (activeCount === 0) {
      return "Accounts configuration error: no active account selected. Mark exactly one account with isActive=true.";
    }
    if (activeCount > 1) {
      return "Accounts configuration error: multiple active accounts selected (" + activeCount + "). Mark exactly one account with isActive=true.";
    }

    return null;
  }

  function discoverLegacyAuthRoute(source, sourceIndex) {
    var authPath = source.authPath;
    var authResult = readJsonFile(authPath, "missing", "auth");
    if (authResult.kind === "missing" || authResult.kind === "empty") {
      return;
    }
    if (authResult.kind === "error") {
      var readErrorId = reserveRouteId("opencode-" + sourceIndex);
      pushErrorRoute(readErrorId, authPath, authResult.error, readErrorId);
      return;
    }

    var copilot = authResult.doc[COPILOT_PROVIDER_KEY];
    if (copilot === undefined) {
      return;
    }

    var parsed = parseCopilotCredential(copilot);
    var routeId = reserveRouteId("opencode-" + sourceIndex);
    if (parsed.error) {
      pushErrorRoute(routeId, authPath, parsed.error, routeId);
      return;
    }

    ROUTE_REGISTRY[routeId] = {
      path: authPath,
      token: parsed.token,
      error: null,
      readSource: "auth"
    };

    accounts.push(buildAccountDescriptor(routeId, "OpenCode " + (accounts.length + 1), routeId));
  }

  function readAccountsEntryData(path, providerKey, index) {
    var docResult = readJsonFile(path, "missing", "accounts");
    if (docResult.kind !== "value") return null;
    var providerEntries = docResult.doc[providerKey];
    if (!Array.isArray(providerEntries)) return null;
    if (index < 0 || index >= providerEntries.length) return null;
    var entry = providerEntries[index];
    if (!isPlainObject(entry)) return null;
    return entry.data;
  }

  for (var si = 0; si < OPENCODE_SOURCES.length; si++) {
    var source = OPENCODE_SOURCES[si];
    var authPath = source.authPath;
    var accountsPath = source.accountsPath;

    var accountsResult = readJsonFile(accountsPath, "missing", "accounts");

    if (accountsResult.kind === "missing" || accountsResult.kind === "empty") {
      discoverLegacyAuthRoute(source, si);
      continue;
    }

    if (accountsResult.kind === "error") {
      var parseErrorId = nextRouteId();
      pushErrorRoute(parseErrorId, accountsPath, accountsResult.error, parseErrorId);
      continue;
    }

    var providerEntries = accountsResult.doc[COPILOT_PROVIDER_KEY];
    if (providerEntries === undefined) {
      discoverLegacyAuthRoute(source, si);
      continue;
    }

    if (!Array.isArray(providerEntries)) {
      var badProviderId = nextRouteId();
      pushErrorRoute(
        badProviderId,
        accountsPath,
        "Accounts configuration error: '" + COPILOT_PROVIDER_KEY + "' must be a list of account entries.",
        badProviderId
      );
      continue;
    }

    var accountsValidationError = validateAccountsEntries(providerEntries);
    if (accountsValidationError) {
      var validationErrorId = reserveRouteId("opencode-" + si);
      pushErrorRoute(validationErrorId, accountsPath, accountsValidationError, validationErrorId);
      continue;
    }

    for (var ai = 0; ai < providerEntries.length; ai++) {
      var entry = providerEntries[ai];

      if (!isPlainObject(entry)) {
        var invalidEntryId = nextRouteId();
        pushErrorRoute(invalidEntryId, accountsPath, "Invalid account entry at index " + ai, invalidEntryId);
        continue;
      }

      var declaredAccountId = isNonEmptyString(entry.accountId) ? entry.accountId.trim() : "";
      if (!declaredAccountId) {
        var missingAccountId = nextRouteId();
        pushErrorRoute(missingAccountId, accountsPath, "Invalid account entry at index " + ai + ": accountId is required", missingAccountId);
        continue;
      }

      var routeId = reserveRouteId(declaredAccountId);

      var isActive = entry.isActive === true;
      var parsed;
      if (isActive) {
        parsed = readAuthToken(authPath);
      } else {
        parsed = parseCopilotCredential(entry.data);
      }

      if (!parsed || parsed.error) {
        pushErrorRoute(routeId, isActive ? authPath : accountsPath, (parsed && parsed.error) || "Invalid github-copilot credentials", declaredAccountId);
        continue;
      }

      ROUTE_REGISTRY[routeId] = {
        path: authPath,
        authPath: authPath,
        accountsPath: accountsPath,
        token: parsed.token,
        error: null,
        readSource: isActive ? "auth" : "accounts",
        accountsIndex: ai
      };

      accounts.push(buildAccountDescriptor(routeId, declaredAccountId, declaredAccountId));
      accounts[accounts.length - 1].isActive = isActive;
    }
  }

  return accounts;
}

// ── AST Patch implementations (direct callbacks) ──────────────────
// Each callback receives (original, ...args) and returns the result
// directly — no factory/handler wrapping.

function patchLoadToken(originalLoadToken, ctx) {
  var accountId = ctx.account && ctx.account.id;

  if (accountId === "default") {
    return originalLoadToken(ctx);
  }

  var route = ROUTE_REGISTRY[accountId];
  if (!route) {
    logWarn(ctx, "copilot override: unknown account route for " + String(accountId));
    throw "Unknown account: " + accountId;
  }
  if (route.error) {
    logWarn(ctx, "copilot override: route error for " + String(accountId) + ": " + route.error);
    throw route.error;
  }

  if (route.readSource === "accounts") {
    var latestData = (function () {
      var fs = ctx && ctx.host && ctx.host.fs;
      if (!fs || typeof fs.readText !== "function") return null;
      var text;
      try {
        text = fs.readText(route.accountsPath);
      } catch (_) {
        return null;
      }
      if (!isNonEmptyString(text)) return null;
      var doc = parseJsonLoose(text);
      if (!isPlainObject(doc)) return null;
      var providerEntries = doc[COPILOT_PROVIDER_KEY];
      if (!Array.isArray(providerEntries)) return null;
      if (route.accountsIndex < 0 || route.accountsIndex >= providerEntries.length) return null;
      var entry = providerEntries[route.accountsIndex];
      if (!isPlainObject(entry)) return null;
      var parsed = parseCopilotCredential(entry.data);
      if (parsed.error) return null;
      return parsed.token;
    })();
    if (isNonEmptyString(latestData)) {
      route.token = latestData;
    }
  }

  return { token: route.token, source: "opencode", authPath: route.path };
}

// Subscribe to candidate paths so the monitor knows about them.
for (var sj = 0; sj < OPENCODE_SOURCES.length; sj++) {
  globalThis.__openusage_ctx.host.fs.subscribeFile(OPENCODE_SOURCES[sj].authPath);
  globalThis.__openusage_ctx.host.fs.subscribeFile(OPENCODE_SOURCES[sj].accountsPath);
}
