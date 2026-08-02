// ── Codex override: account discovery + source routing ────────────
// This override adds opencode-* accounts to the Codex plugin.
// It patches loadAuth, saveAuth, and refreshToken to route through
// exact selected credential sources with no cross-file or native fallback.

globalThis.__openusage_ast_patch = {
  functions: [
    { target: "loadAuth", with: "patchLoadAuth", mode: "wrap" },
    { target: "saveAuth", with: "patchSaveAuth", mode: "wrap" },
    { target: "refreshToken", with: "patchRefreshToken", mode: "wrap" }
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
    if (ctx && ctx.host && typeof ctx.host.log === "function") {
      ctx.host.log("warn", msg);
    }
  } catch (_) {}
}

function parseJsonStrict(text) {
  if (!isNonEmptyString(text)) return null;
  return JSON.parse(text);
}

function parseCredentialObject(block) {
  if (!isPlainObject(block)) {
    return { error: "Provider block is not an object" };
  }

  var accessToken = isNonEmptyString(block.access) ? block.access.trim() : "";
  if (!accessToken) {
    return { error: "Provider block has no access token" };
  }

  var refreshToken = isNonEmptyString(block.refresh) ? block.refresh.trim() : "";
  var accountIdFromBlock = isNonEmptyString(block.accountId) ? block.accountId.trim() : "";

  return {
    credential: {
      access_token: accessToken,
      refresh_token: refreshToken
    },
    accountIdFromBlock: accountIdFromBlock
  };
}

function buildAuthStateFromCredential(cred, route) {
  var auth = {
    last_refresh: new Date().toISOString(),
    tokens: {
      access_token: cred.access_token || "",
      refresh_token: cred.refresh_token || ""
    }
  };
  if (route.accountId) {
    auth.tokens.account_id = route.accountId;
  }
  return {
    source: "opencode",
    auth: auth,
    authPath: route.path,
    providerKey: route.providerKey
  };
}

// ── Route registry ────────────────────────────────────────────────
// Populated by discoverAccounts. Each entry maps accountId -> route.
var routes = {};

// ── discoverAccounts ──────────────────────────────────────────────
// Scans OpenCode auth/accounts candidates and yields one account
// per resolved route.

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
var OPENAI_PROVIDER_KEY = "openai";

function discoverAccounts(ctx, originalDiscoverAccounts) {
  var keys = Object.keys(routes);
  for (var k = 0; k < keys.length; k++) {
    delete routes[keys[k]];
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

  function pushErrorRoute(routeId, message, label) {
    routes[routeId] = { type: "error", error: message };
    accounts.push(buildAccountDescriptor(routeId, label || routeId));
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

    var doc;
    try {
      doc = parseJsonStrict(text);
    } catch (e) {
      return { kind: "error", error: "Invalid JSON in " + kindLabel + " file: " + String(e) };
    }

    if (!isPlainObject(doc)) {
      return { kind: "error", error: kindLabel + " file is not a valid JSON object" };
    }

    return { kind: "value", doc: doc };
  }

  function readAuthCredential(authPath) {
    var authResult = readJsonFile(authPath, "missing", "auth");
    if (authResult.kind !== "value") {
      if (authResult.kind === "missing" || authResult.kind === "empty") {
        return { error: "Auth file is missing or empty" };
      }
      return { error: authResult.error };
    }

    var block = authResult.doc[OPENAI_PROVIDER_KEY];
    if (block === undefined) {
      return { error: "Provider block has no access token" };
    }

    var parsed = parseCredentialObject(block);
    if (parsed.error) {
      return { error: parsed.error };
    }

    return {
      credential: parsed.credential,
      accountIdFromBlock: parsed.accountIdFromBlock
    };
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
      var errorId = reserveRouteId("opencode-" + sourceIndex);
      pushErrorRoute(errorId, authResult.error, errorId);
      return;
    }

    var block = authResult.doc[OPENAI_PROVIDER_KEY];
    if (block === undefined) {
      return;
    }

    var parsed = parseCredentialObject(block);
    var routeId = reserveRouteId("opencode-" + sourceIndex);
    if (parsed.error) {
      pushErrorRoute(routeId, parsed.error, routeId);
      return;
    }

    var legacyAccountId = parsed.accountIdFromBlock || routeId;
    routes[routeId] = {
      path: authPath,
      providerKey: OPENAI_PROVIDER_KEY,
      accountId: parsed.accountIdFromBlock,
      credential: parsed.credential,
      type: "ready",
      persistFailed: false,
      readSource: "auth",
      persistSource: "auth"
    };
    accounts.push(buildAccountDescriptor(routeId, "OpenCode " + (accounts.length + 1), legacyAccountId));
  }

  for (var i = 0; i < OPENCODE_SOURCES.length; i++) {
    var source = OPENCODE_SOURCES[i];
    var authPath = source.authPath;
    var accountsPath = source.accountsPath;

    var accountsResult = readJsonFile(accountsPath, "missing", "accounts");

    if (accountsResult.kind === "missing" || accountsResult.kind === "empty") {
      discoverLegacyAuthRoute(source, i);
      continue;
    }

    if (accountsResult.kind === "error") {
      var parseErrorId = nextRouteId();
      pushErrorRoute(parseErrorId, accountsResult.error, parseErrorId);
      continue;
    }

    var providerEntries = accountsResult.doc[OPENAI_PROVIDER_KEY];
    if (providerEntries === undefined) {
      discoverLegacyAuthRoute(source, i);
      continue;
    }

    if (!Array.isArray(providerEntries)) {
      var badProviderId = nextRouteId();
      pushErrorRoute(
        badProviderId,
        "Accounts configuration error: '" + OPENAI_PROVIDER_KEY + "' must be a list of account entries.",
        badProviderId
      );
      continue;
    }

    var accountsValidationError = validateAccountsEntries(providerEntries);
    if (accountsValidationError) {
      var validationErrorId = reserveRouteId("opencode-" + i);
      pushErrorRoute(validationErrorId, accountsValidationError, validationErrorId);
      continue;
    }

    for (var j = 0; j < providerEntries.length; j++) {
      var entry = providerEntries[j];

      if (!isPlainObject(entry)) {
        var routeId = nextRouteId();
        pushErrorRoute(routeId, "Invalid account entry at index " + j, routeId);
        continue;
      }

      var declaredAccountId = isNonEmptyString(entry.accountId) ? entry.accountId.trim() : "";
      if (!declaredAccountId) {
        var missingAccountIdRoute = nextRouteId();
        pushErrorRoute(
          missingAccountIdRoute,
          "Invalid account entry at index " + j + ": accountId is required",
          missingAccountIdRoute
        );
        continue;
      }

      if (usedRouteIds[declaredAccountId]) {
        var duplicateIdRoute = nextRouteId();
        pushErrorRoute(
          duplicateIdRoute,
          "Duplicate accountId in discovery results: " + declaredAccountId,
          declaredAccountId
        );
        continue;
      }

      var routeId = reserveRouteId(declaredAccountId);

      var isActive = entry.isActive === true;
      var parsed;

      if (isActive) {
        parsed = readAuthCredential(authPath);
        if (parsed.error) {
          pushErrorRoute(routeId, parsed.error, declaredAccountId);
          continue;
        }
      } else {
        parsed = parseCredentialObject(entry.data);
        if (parsed.error) {
          pushErrorRoute(routeId, parsed.error, declaredAccountId);
          continue;
        }
      }

      routes[routeId] = {
        path: authPath,
        accountsPath: accountsPath,
        providerKey: OPENAI_PROVIDER_KEY,
        accountId: declaredAccountId,
        credential: parsed.credential,
        type: "ready",
        persistFailed: false,
        readSource: isActive ? "auth" : "accounts",
        persistSource: isActive ? "auth" : "accounts",
        accountsIndex: j
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

function patchLoadAuth(originalLoadAuth, ctx) {
  var accountId = ctx && ctx.account && ctx.account.id;

  if (!accountId || accountId === "default") {
    return originalLoadAuth(ctx);
  }

  var route = routes[accountId];
  if (!route) {
    throw "Unknown account: " + accountId;
  }

  if (route.type === "error") {
    throw route.error;
  }

  if (route.persistFailed) {
    throw "Token persistence failed for account " + accountId;
  }

  return buildAuthStateFromCredential(route.credential, route);
}

function patchSaveAuth(originalSaveAuth, ctx, authState) {
  var accountId = ctx && ctx.account && ctx.account.id;

  if (!accountId || accountId === "default") {
    return originalSaveAuth(ctx, authState);
  }

  var route = routes[accountId];
  if (!route) {
    throw "Unknown account: " + accountId;
  }

  if (route.type === "error") {
    throw route.error;
  }

  if (route.persistFailed) {
    throw "Token persistence failed for account " + accountId;
  }

  var tokens = authState && authState.auth && authState.auth.tokens;
  if (!tokens) return false;

  var newTokens = {};
  if (isNonEmptyString(tokens.access_token)) {
    newTokens.access_token = tokens.access_token;
  }
  if (isNonEmptyString(tokens.refresh_token)) {
    newTokens.refresh_token = tokens.refresh_token;
  }

  var saved = persistRouteTokens(ctx, route, newTokens);
  if (!saved) {
    route.persistFailed = true;
    logWarn(ctx, "codex override: failed to persist auth for account " + accountId);
  } else {
    if (newTokens.access_token) route.credential.access_token = newTokens.access_token;
    if (newTokens.refresh_token) route.credential.refresh_token = newTokens.refresh_token;
    route.persistFailed = false;
  }
  return saved;
}

function patchRefreshToken(originalRefreshToken, ctx, authState) {
  var accountId = ctx && ctx.account && ctx.account.id;

  if (!accountId || accountId === "default") {
    return originalRefreshToken(ctx, authState);
  }

  var route = routes[accountId];
  if (!route) {
    throw "Unknown account: " + accountId;
  }

  if (route.type === "error") {
    throw route.error;
  }

  if (route.persistFailed) {
    throw "Token persistence failed for account " + accountId;
  }

  var priorToken = route.credential && route.credential.access_token;

  var reloadedCredential = reloadCredentialFromRoute(ctx, route);
  if (!reloadedCredential) {
    throw "Failed to reload auth file for account " + accountId;
  }

  var currentAccessToken = isNonEmptyString(reloadedCredential.access_token)
    ? reloadedCredential.access_token.trim()
    : "";

  if (isNonEmptyString(currentAccessToken) && currentAccessToken !== priorToken) {
    route.credential.access_token = currentAccessToken;
    if (isNonEmptyString(reloadedCredential.refresh_token)) {
      route.credential.refresh_token = reloadedCredential.refresh_token;
    }
    if (authState && authState.auth && authState.auth.tokens) {
      authState.auth.tokens.access_token = currentAccessToken;
      if (isNonEmptyString(reloadedCredential.refresh_token)) {
        authState.auth.tokens.refresh_token = reloadedCredential.refresh_token;
      }
    }
    return currentAccessToken;
  }

  var upstreamRefreshResult = originalRefreshToken(ctx, authState);

  if (authState && authState.auth && authState.auth.tokens) {
    var upstreamAccess = authState.auth.tokens.access_token;
    if (isNonEmptyString(upstreamAccess)) {
      route.credential.access_token = upstreamAccess;
    }
    var upstreamRefresh = authState.auth.tokens.refresh_token;
    if (isNonEmptyString(upstreamRefresh)) {
      route.credential.refresh_token = upstreamRefresh;
    }
  }

  if (route.persistFailed) {
    throw "Token refresh succeeded but persistence failed for account " + accountId;
  }

  return upstreamRefreshResult;
}

// ── File persistence helpers ──────────────────────────────────────

function readProviderBlock(ctx, path, providerKey) {
  var fs = ctx && ctx.host && ctx.host.fs;
  if (!fs || typeof fs.readText !== "function") return null;

  var text;
  try {
    text = fs.readText(path);
  } catch (_) {
    return null;
  }

  if (!isNonEmptyString(text)) return null;

  var doc;
  try {
    doc = JSON.parse(text);
  } catch (_) {
    return null;
  }

  if (!doc || typeof doc !== "object" || Array.isArray(doc)) return null;

  var block = doc[providerKey];
  if (!block || typeof block !== "object" || Array.isArray(block)) return null;

  return block;
}

function readAccountsEntryData(ctx, path, providerKey, index) {
  var fs = ctx && ctx.host && ctx.host.fs;
  if (!fs || typeof fs.readText !== "function") return null;

  var text;
  try {
    text = fs.readText(path);
  } catch (_) {
    return null;
  }

  if (!isNonEmptyString(text)) return null;

  var doc;
  try {
    doc = JSON.parse(text);
  } catch (_) {
    return null;
  }

  if (!isPlainObject(doc)) return null;

  var providerEntries = doc[providerKey];
  if (!Array.isArray(providerEntries)) return null;
  if (index < 0 || index >= providerEntries.length) return null;

  var entry = providerEntries[index];
  if (!isPlainObject(entry)) return null;

  return entry.data;
}

function reloadCredentialFromRoute(ctx, route) {
  if (route.readSource === "accounts") {
    var data = readAccountsEntryData(ctx, route.accountsPath, route.providerKey, route.accountsIndex);
    var parsedData = parseCredentialObject(data);
    if (parsedData.error) {
      return null;
    }
    return parsedData.credential;
  }

  var block = readProviderBlock(ctx, route.path, route.providerKey);
  var parsedBlock = parseCredentialObject(block);
  if (parsedBlock.error) {
    return null;
  }
  return parsedBlock.credential;
}

function persistToAuthFile(ctx, path, providerKey, tokens) {
  var fs = ctx && ctx.host && ctx.host.fs;
  if (!fs || typeof fs.readText !== "function" || typeof fs.writeText !== "function") {
    return false;
  }

  var text;
  try {
    text = fs.readText(path);
  } catch (_) {
    return false;
  }

  if (!isNonEmptyString(text)) return false;

  var doc;
  try {
    doc = JSON.parse(text);
  } catch (_) {
    return false;
  }

  if (!doc || typeof doc !== "object" || Array.isArray(doc)) return false;

  var block = doc[providerKey];
  if (!block || typeof block !== "object" || Array.isArray(block)) {
    return false;
  }

  if (isNonEmptyString(tokens.access_token)) {
    block.access = tokens.access_token;
  }
  if (isNonEmptyString(tokens.refresh_token)) {
    block.refresh = tokens.refresh_token;
  }
  block.type = "oauth";
  if (typeof block.expires !== "number") {
    block.expires = 0;
  }

  doc[providerKey] = block;

  try {
    fs.writeText(path, JSON.stringify(doc, null, 2));
    return true;
  } catch (_) {
    return false;
  }
}

function persistToAccountsFile(ctx, path, providerKey, index, tokens, declaredAccountId) {
  var fs = ctx && ctx.host && ctx.host.fs;
  if (!fs || typeof fs.readText !== "function" || typeof fs.writeText !== "function") {
    return false;
  }

  var text;
  try {
    text = fs.readText(path);
  } catch (_) {
    return false;
  }

  if (!isNonEmptyString(text)) return false;

  var doc;
  try {
    doc = JSON.parse(text);
  } catch (_) {
    return false;
  }

  if (!isPlainObject(doc)) return false;

  var providerEntries = doc[providerKey];
  if (!Array.isArray(providerEntries)) return false;
  if (index < 0 || index >= providerEntries.length) return false;

  var entry = providerEntries[index];
  if (!isPlainObject(entry)) return false;

  if (!isPlainObject(entry.data)) {
    entry.data = {};
  }

  if (isNonEmptyString(tokens.access_token)) {
    entry.data.access = tokens.access_token;
  }
  if (isNonEmptyString(tokens.refresh_token)) {
    entry.data.refresh = tokens.refresh_token;
  }
  entry.data.type = "oauth";
  if (typeof entry.data.expires !== "number") {
    entry.data.expires = 0;
  }
  if (isNonEmptyString(declaredAccountId)) {
    entry.data.accountId = declaredAccountId;
  }

  providerEntries[index] = entry;
  doc[providerKey] = providerEntries;

  try {
    fs.writeText(path, JSON.stringify(doc, null, 2));
    return true;
  } catch (_) {
    return false;
  }
}

function persistRouteTokens(ctx, route, tokens) {
  if (route.persistSource === "accounts") {
    return persistToAccountsFile(
      ctx,
      route.accountsPath,
      route.providerKey,
      route.accountsIndex,
      tokens,
      route.accountId
    );
  }

  return persistToAuthFile(ctx, route.path, route.providerKey, tokens);
}

// ── Subscribe to candidate paths ─────────────────────────────────
for (var si = 0; si < OPENCODE_SOURCES.length; si++) {
  globalThis.__openusage_ctx.host.fs.subscribeFile(OPENCODE_SOURCES[si].authPath);
  globalThis.__openusage_ctx.host.fs.subscribeFile(OPENCODE_SOURCES[si].accountsPath);
}
