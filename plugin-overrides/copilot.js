// ── Copilot override: account discovery + source routing ──────────
// This override adds opencode-<path-index> accounts to the Copilot plugin.
// It patches loadToken to route through exact selected auth files with
// no cross-file or native fallback.

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

function parseJsonLoose(text) {
  if (!isNonEmptyString(text)) return null;
  try {
    return JSON.parse(text);
  } catch (_) {
    return null;
  }
}

// ── Route registry ────────────────────────────────────────────────
// Populated by discoverAccounts. Each entry maps accountId -> route.

var ROUTE_REGISTRY = {};

var COPILOT_PROVIDER_KEY = "github-copilot";

// ── discoverAccounts ──────────────────────────────────────────────
// Scans OPENCODE_AUTH_PATHS for valid auth files and yields one
// account per candidate. Each account is identified by its path index.

var OPENCODE_AUTH_PATHS = [
  "~/.local/share/opencode/auth.json",
  "~/.config/opencode/auth.json"
];

function discoverAccounts(ctx, originalDiscoverAccounts) {
  // Reset routes on each discovery
  var keys = Object.keys(ROUTE_REGISTRY);
  for (var i = 0; i < keys.length; i++) {
    delete ROUTE_REGISTRY[keys[i]];
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
        fileExists = null;
      }
    } catch (e) {
      ROUTE_REGISTRY[accountId] = { path: path, token: null, error: "Failed to check auth file: " + String(e) };
      accounts.push({ id: accountId, origin: "opencode" });
      continue;
    }

    if (fileExists === false) {
      continue;
    }

    var text;
    try {
      text = fs.readText(path);
    } catch (e) {
      ROUTE_REGISTRY[accountId] = { path: path, token: null, error: "Failed to read auth file: " + String(e) };
      accounts.push({ id: accountId, origin: "opencode" });
      continue;
    }

    if (!isNonEmptyString(text)) {
      continue;
    }

    var doc = parseJsonLoose(text);
    if (!doc || typeof doc !== "object" || Array.isArray(doc)) {
      ROUTE_REGISTRY[accountId] = { path: path, token: null, error: "Invalid auth file" };
      accounts.push({ id: accountId, origin: "opencode" });
      continue;
    }

    var copilot = doc[COPILOT_PROVIDER_KEY];
    if (copilot === undefined) {
      // No matching provider key — skip silently
      continue;
    }
    if (!copilot || typeof copilot !== "object" || Array.isArray(copilot)) {
      // Provider key exists but is not a valid object — error account
      ROUTE_REGISTRY[accountId] = { path: path, token: null, error: "Invalid github-copilot credentials" };
      accounts.push({ id: accountId, origin: "opencode" });
      continue;
    }

    var token = isNonEmptyString(copilot.access) ? copilot.access.trim() : "";
    if (!token) {
      ROUTE_REGISTRY[accountId] = { path: path, token: null, error: "Invalid github-copilot credentials" };
      accounts.push({ id: accountId, origin: "opencode" });
      continue;
    }

    ROUTE_REGISTRY[accountId] = {
      path: path,
      token: token,
      error: null
    };

    accounts.push({ id: accountId, origin: "opencode" });
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
    throw "Unknown account: " + accountId;
  }
  if (route.error) {
    throw route.error;
  }

  return { token: route.token, source: "opencode", authPath: route.path };
}

// Subscribe to candidate paths so the monitor knows about them.
for (var si = 0; si < OPENCODE_AUTH_PATHS.length; si++) {
  globalThis.__openusage_ctx.host.fs.subscribeFile(OPENCODE_AUTH_PATHS[si]);
}
