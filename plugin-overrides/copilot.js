globalThis.__openusage_ast_patch = {
  functions: [
    // loadToken is patched to add opencode auth.json fallback sources.
    { target: "loadToken", with: "patchLoadToken", mode: "wrap" },
    // saveToken is patched so tokens loaded from opencode are persisted back to
    // the same auth.json file, not to the plugin-local cache file.
    { target: "saveToken", with: "patchSaveToken", mode: "wrap" },
  ],
};

const OPENCODE_AUTH_PATHS = [
  "~/.local/share/opencode/auth.json",
  "~/.config/opencode/auth.json",
];
const COPILOT_PROVIDER_KEY = "github-copilot";
// The patched wrappers do not share local scope with each other after
// transformation, so we keep minimal cross-call state on globalThis.
const COPILOT_OVERRIDE_STATE_KEY = "__openusage_copilot_override_state";

function patchLoadToken(originalLoadToken, ctx) {
  const primary = originalLoadToken(ctx);
  if (primary && isNonEmptyString(primary.token)) {
    // Remember where the active credential came from. This is later used by
    // patchSaveToken to decide where a refreshed token should be written.
    setActiveCredential(primary);
    return primary;
  }

  const fallback = loadOpencodeTokenFallback(ctx);
  if (fallback) {
    setActiveCredential(fallback);
    return fallback;
  }

  setActiveCredential(null);
  return primary;
}

function patchSaveToken(originalSaveToken, ctx, token) {
  const trimmedToken = isNonEmptyString(token) ? token.trim() : "";
  const activeCredential = getActiveCredential();
  if (
    trimmedToken &&
    activeCredential &&
    activeCredential.source === "opencode" &&
    isNonEmptyString(activeCredential.authPath)
  ) {
    const persisted = persistTokenToOpencode(ctx, activeCredential.authPath, trimmedToken);
    if (persisted) {
      logInfo(ctx, "copilot override: persisted token to " + activeCredential.authPath);
      // Intentionally skip original saveToken: it writes to plugin-local state,
      // which would diverge from the opencode auth source we are using.
      return;
    }
    logWarn(
      ctx,
      "copilot override: failed to persist token to opencode auth, falling back to plugin storage",
    );
  }

  return originalSaveToken(ctx, token);
}

function logInfo(ctx, message) {
  try {
    if (ctx && ctx.host && ctx.host.log && typeof ctx.host.log.info === "function") {
      ctx.host.log.info(message);
    }
  } catch (_) {}
}

function logWarn(ctx, message) {
  try {
    if (ctx && ctx.host && ctx.host.log && typeof ctx.host.log.warn === "function") {
      ctx.host.log.warn(message);
    }
  } catch (_) {}
}

function isNonEmptyString(value) {
  return typeof value === "string" && value.trim().length > 0;
}

function parseJsonLoose(text) {
  if (typeof text !== "string") {
    return null;
  }
  const trimmed = text.replace(/\u0000+$/g, "").trim();
  if (!trimmed) {
    return null;
  }
  try {
    return JSON.parse(trimmed);
  } catch (_) {
    return null;
  }
}

function buildCopilotTokenFromOpencodeDoc(doc) {
  if (!doc || typeof doc !== "object") {
    return null;
  }

  const copilot = doc[COPILOT_PROVIDER_KEY];
  if (!copilot || typeof copilot !== "object") {
    return null;
  }

  const accessToken = isNonEmptyString(copilot.access) ? copilot.access.trim() : "";
  if (!accessToken) {
    return null;
  }

  return accessToken;
}

function persistTokenToOpencode(ctx, authPath, accessToken) {
  if (!ctx || !ctx.host || !ctx.host.fs) {
    return false;
  }
  if (!isNonEmptyString(authPath) || !isNonEmptyString(accessToken)) {
    return false;
  }

  const fs = ctx.host.fs;
  if (typeof fs.readText !== "function" || typeof fs.writeText !== "function") {
    return false;
  }

  let doc = {};
  try {
    if (fs.exists(authPath)) {
      doc = parseJsonLoose(fs.readText(authPath)) || {};
    }
  } catch (_) {
    doc = {};
  }
  if (!doc || typeof doc !== "object") {
    doc = {};
  }

  let copilot = doc[COPILOT_PROVIDER_KEY];
  if (!copilot || typeof copilot !== "object") {
    copilot = {};
  }

  // Update only the provider block we own and keep other providers unchanged.
  copilot.access = accessToken;
  if (!isNonEmptyString(copilot.type)) {
    copilot.type = "oauth";
  }
  if (typeof copilot.expires !== "number") {
    copilot.expires = 0;
  }

  doc[COPILOT_PROVIDER_KEY] = copilot;
  fs.writeText(authPath, JSON.stringify(doc, null, 2));
  return true;
}

function getActiveCredential() {
  const state = globalThis[COPILOT_OVERRIDE_STATE_KEY];
  if (!state || typeof state !== "object") {
    return null;
  }
  return state.activeCredential || null;
}

function setActiveCredential(credential) {
  let state = globalThis[COPILOT_OVERRIDE_STATE_KEY];
  if (!state || typeof state !== "object") {
    state = {};
    globalThis[COPILOT_OVERRIDE_STATE_KEY] = state;
  }

  if (!credential || typeof credential !== "object") {
    state.activeCredential = null;
    return;
  }

  // Keep only routing metadata. Token text is intentionally not duplicated.
  state.activeCredential = {
    source: credential.source,
    authPath: credential.authPath,
  };
}

function loadOpencodeTokenAtPath(ctx, authPath) {
  if (!ctx || !ctx.host || !ctx.host.fs || !isNonEmptyString(authPath)) {
    return null;
  }

  if (!ctx.host.fs.exists(authPath)) {
    return null;
  }

  const text = ctx.host.fs.readText(authPath);
  const doc = parseJsonLoose(text);
  if (!doc) {
    logWarn(ctx, "copilot override: opencode auth file is invalid JSON: " + authPath);
    return null;
  }

  const token = buildCopilotTokenFromOpencodeDoc(doc);
  if (!token) {
    logWarn(
      ctx,
      "copilot override: github-copilot auth payload not found in " + authPath,
    );
    return null;
  }

  return {
    token: token,
    source: "opencode",
    authPath: authPath,
  };
}

function loadOpencodeTokenFallback(ctx) {
  if (!ctx || !ctx.host || !ctx.host.fs) {
    return null;
  }

  // Keep path order explicit and deterministic so behavior is predictable.
  for (let i = 0; i < OPENCODE_AUTH_PATHS.length; i++) {
    const authPath = OPENCODE_AUTH_PATHS[i];
    try {
      const fallback = loadOpencodeTokenAtPath(ctx, authPath);
      if (!fallback) {
        continue;
      }

      logInfo(ctx, "copilot override: using fallback auth from " + authPath);
      return fallback;
    } catch (e) {
      logWarn(ctx, "copilot override: failed to read fallback auth from " + authPath + ": " + String(e));
    }
  }

  return null;
}
