// ── Codex override: account discovery + source routing ────────────
// This override adds opencode-<path-index> accounts to the Codex plugin.
// It patches loadAuth, saveAuth, and refreshToken to route through
// exact selected auth files with no cross-file or native fallback.

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

function logWarn(ctx, msg) {
  try {
    if (ctx && ctx.host && typeof ctx.host.log === "function") {
      ctx.host.log("warn", msg);
    }
  } catch (_) {}
}

function buildAuthStateFromCredential(cred, route) {
  var auth = {
    last_refresh: new Date().toISOString(),
    tokens: {
      access_token: cred.access_token || "",
      refresh_token: cred.refresh_token || "",
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
// Scans OPENCODE_AUTH_PATHS for valid auth files and yields one
// account per candidate. Each account is identified by its path index.

var OPENCODE_AUTH_PATHS = [
  "~/.local/share/opencode/auth.json",
  "~/.config/opencode/auth.json"
];
var OPENAI_PROVIDER_KEY = "openai";

function discoverAccounts(ctx, originalDiscoverAccounts) {
  // Reset routes on each discovery
  var keys = Object.keys(routes);
  for (var i = 0; i < keys.length; i++) {
    delete routes[keys[i]];
  }

  var accounts = [];

  // Include original plugin accounts first
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
    // No original discoverAccounts — add a default account with
    // hide-if-other-account policy so errors are suppressed when
    // opencode accounts exist.
    accounts.push({
      id: "default",
      errorPolicy: "hide-if-other-account"
    });
  }

  var fs = ctx && ctx.host && ctx.host.fs;

  function buildAccountDescriptor(index, stableSubjectKey) {
    var d = {
      id: "opencode-" + index,
      origin: "opencode",
      name: "OpenCode " + (index + 1),
      sourceRef: "opencode-auth:path-index-" + index
    };
    if (stableSubjectKey && typeof stableSubjectKey === "string") {
      d.stableSubjectKey = stableSubjectKey;
    }
    return d;
  }

  for (var i = 0; i < OPENCODE_AUTH_PATHS.length; i++) {
    var path = OPENCODE_AUTH_PATHS[i];
    var accountId = "opencode-" + i;

    if (!fs || typeof fs.readText !== "function") {
      continue;
    }

    // Check existence first to distinguish missing from unreadable.
    var fileExists;
    try {
      if (typeof fs.exists === "function") {
        fileExists = fs.exists(path);
      } else {
        // No exists API — fall back to try-readText
        fileExists = null;
      }
    } catch (e) {
      // exists() threw — treat as error route
      routes[accountId] = { type: "error", error: "Failed to check auth file: " + String(e) };
      accounts.push(buildAccountDescriptor(i));
      continue;
    }

    if (fileExists === false) {
      // File does not exist — omit this account
      continue;
    }

    var text;
    try {
      text = fs.readText(path);
    } catch (e) {
      // File exists but read failed — error route
      routes[accountId] = { type: "error", error: "Failed to read auth file: " + String(e) };
      accounts.push(buildAccountDescriptor(i));
      continue;
    }

    if (!isNonEmptyString(text)) {
      continue;
    }

    var doc;
    try {
      doc = JSON.parse(text);
    } catch (e) {
      routes[accountId] = { type: "error", error: "Invalid JSON: " + String(e) };
      accounts.push(buildAccountDescriptor(i));
      continue;
    }

    if (!doc || typeof doc !== "object" || Array.isArray(doc)) {
      routes[accountId] = { type: "error", error: "Auth file is not valid JSON" };
      accounts.push(buildAccountDescriptor(i));
      continue;
    }

    // Find the first provider block with an access token
    var providerKeys = Object.keys(doc);
    var found = false;

    for (var j = 0; j < providerKeys.length; j++) {
      var key = providerKeys[j];
      var block = doc[key];

      // Only accept openai provider blocks
      if (key !== OPENAI_PROVIDER_KEY) {
        continue;
      }

      // Provider block exists but is not an object — error account
      if (!block || typeof block !== "object" || Array.isArray(block)) {
        routes[accountId] = { type: "error", error: "Provider block is not an object" };
        accounts.push(buildAccountDescriptor(i));
        found = true;
        break;
      }

      var accessToken = isNonEmptyString(block.access) ? block.access.trim() : "";
      if (!accessToken) {
        // Provider block exists but no access token — error account
        routes[accountId] = { type: "error", error: "Provider block has no access token" };
        accounts.push(buildAccountDescriptor(i));
        found = true;
        break;
      }

      var refreshToken = isNonEmptyString(block.refresh) ? block.refresh.trim() : "";
      var accountIdFromBlock = isNonEmptyString(block.accountId) ? block.accountId : "";

      // Store route for this account
      routes[accountId] = {
        path: path,
        providerKey: key,
        accountId: accountIdFromBlock,
        credential: {
          access_token: accessToken,
          refresh_token: refreshToken
        },
        type: "ready",
        persistFailed: false
      };

      accounts.push(buildAccountDescriptor(i, accountIdFromBlock || accountId));
      found = true;
      break;
    }

    if (!found) {
      continue;
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

  var saved = persistToFile(ctx, route.path, route.providerKey, newTokens);
  if (!saved) {
    route.persistFailed = true;
    logWarn(ctx, "codex override: failed to persist auth to " + route.path);
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

  // Capture in-memory token before exact-source reload
  var priorToken = route.credential && route.credential.access_token;

  var providerBlock = readProviderBlock(ctx, route.path, route.providerKey);
  if (!providerBlock) {
    throw "Failed to reload auth file for account " + accountId;
  }

  var currentAccessToken = isNonEmptyString(providerBlock.access)
    ? providerBlock.access.trim()
    : "";

  // If token changed on disk since last load, use it immediately
  // without OAuth refresh, regardless of last_refresh age.
  if (isNonEmptyString(currentAccessToken) && currentAccessToken !== priorToken) {
    route.credential.access_token = currentAccessToken;
    if (authState && authState.auth && authState.auth.tokens) {
      authState.auth.tokens.access_token = currentAccessToken;
    }
    return currentAccessToken;
  }

  // No changed token — delegate to upstream OAuth refresh.
  var upstreamRefreshResult = originalRefreshToken(ctx, authState);

  // Sync route credential state from the mutated authState after upstream refresh.
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

  // If selected-source save failed earlier, surface it now as an
  // account-specific error — even though upstream may have succeeded.
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

function persistToFile(ctx, path, providerKey, tokens) {
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
    // Provider block missing — cannot persist
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

// ── Subscribe to candidate paths ─────────────────────────────────
// Subscribe to candidate paths so the monitor knows about them.
for (var si = 0; si < OPENCODE_AUTH_PATHS.length; si++) {
  globalThis.__openusage_ctx.host.fs.subscribeFile(OPENCODE_AUTH_PATHS[si]);
}
