globalThis.__openusage_ast_patch = {
  functions: [
    { target: "loadAuth", with: "patchLoadAuth", mode: "wrap" },
    { target: "saveAuth", with: "patchSaveAuth", mode: "wrap" },
    { target: "refreshToken", with: "patchRefreshToken", mode: "wrap" },
  ],
};

const OPENCODE_AUTH_PATHS = [
  "~/.local/share/opencode/auth.json",
  "~/.config/opencode/auth.json",
];
const OPENAI_PROVIDER_KEY = "openai";

function patchLoadAuth(originalLoadAuth, ctx) {
  const primary = originalLoadAuth(ctx);
  if (primary) {
    return primary;
  }
  return loadOpencodeAuthFallback(ctx);
}

function patchSaveAuth(originalSaveAuth, ctx, authState) {
  if (authState && authState.source === "opencode") {
    return persistAuthToOpencode(ctx, authState);
  }
  return originalSaveAuth(ctx, authState);
}

function patchRefreshToken(originalRefreshToken, ctx, authState) {
  if (!authState || authState.source !== "opencode") {
    return originalRefreshToken(ctx, authState);
  }

  const currentAccessToken = readAccessToken(authState);
  const latestAuthState = reloadOpencodeAuthState(ctx, authState);
  if (latestAuthState) {
    applyAuthState(authState, latestAuthState);
    const reloadedAccessToken = readAccessToken(authState);
    if (
      isNonEmptyString(reloadedAccessToken) &&
      (!isNonEmptyString(currentAccessToken) || reloadedAccessToken !== currentAccessToken)
    ) {
      logInfo(ctx, "codex override: token reloaded from opencode auth.json, refresh skipped");
      return reloadedAccessToken;
    }
  }

  return originalRefreshToken(ctx, authState);
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

function isNonEmptyString(value) {
  return typeof value === "string" && value.trim().length > 0;
}

function nowIso() {
  return new Date().toISOString();
}

function readAccessToken(authState) {
  if (!authState || !authState.auth || !authState.auth.tokens) {
    return "";
  }
  const accessToken = authState.auth.tokens.access_token;
  return isNonEmptyString(accessToken) ? accessToken.trim() : "";
}

function applyAuthState(targetAuthState, latestAuthState) {
  if (!targetAuthState || !latestAuthState) {
    return;
  }
  targetAuthState.source = latestAuthState.source;
  targetAuthState.authPath = latestAuthState.authPath;
  targetAuthState.auth = latestAuthState.auth;
}

function buildCodexAuthStateFromOpencodeDoc(doc, sourcePath) {
  if (!doc || typeof doc !== "object") {
    return null;
  }

  const openai = doc[OPENAI_PROVIDER_KEY];
  if (!openai || typeof openai !== "object") {
    return null;
  }

  const accessToken = isNonEmptyString(openai.access) ? openai.access.trim() : "";
  if (!accessToken) {
    return null;
  }

  const tokens = {
    access_token: accessToken,
  };
  if (isNonEmptyString(openai.refresh)) {
    tokens.refresh_token = openai.refresh.trim();
  }
  if (isNonEmptyString(openai.accountId)) {
    tokens.account_id = openai.accountId.trim();
  }

  return {
    source: "opencode",
    authPath: sourcePath,
    auth: {
      tokens: tokens,
      last_refresh: nowIso(),
    },
  };
}

function loadOpencodeAuthAtPath(ctx, authPath) {
  if (!ctx || !ctx.host || !ctx.host.fs || !isNonEmptyString(authPath)) {
    return null;
  }

  if (!ctx.host.fs.exists(authPath)) {
    return null;
  }

  const text = ctx.host.fs.readText(authPath);
  const doc = parseJsonLoose(text);
  if (!doc) {
    logWarn(ctx, "codex override: opencode auth file is invalid JSON: " + authPath);
    return null;
  }

  const authState = buildCodexAuthStateFromOpencodeDoc(doc, authPath);
  if (!authState) {
    logWarn(ctx, "codex override: openai auth payload not found in " + authPath);
    return null;
  }

  return authState;
}

function reloadOpencodeAuthState(ctx, authState) {
  if (!ctx || !ctx.host || !ctx.host.fs) {
    return null;
  }

  if (authState && isNonEmptyString(authState.authPath)) {
    try {
      const fromCurrentPath = loadOpencodeAuthAtPath(ctx, authState.authPath);
      if (fromCurrentPath) {
        return fromCurrentPath;
      }
    } catch (e) {
      logWarn(
        ctx,
        "codex override: failed to reload opencode auth from " + authState.authPath + ": " + String(e)
      );
    }
  }

  return loadOpencodeAuthFallback(ctx);
}

function loadOpencodeAuthFallback(ctx) {
  if (!ctx || !ctx.host || !ctx.host.fs) {
    return null;
  }

  for (let i = 0; i < OPENCODE_AUTH_PATHS.length; i++) {
    const authPath = OPENCODE_AUTH_PATHS[i];
    try {
      const fallback = loadOpencodeAuthAtPath(ctx, authPath);
      if (!fallback) {
        continue;
      }

      logInfo(ctx, "codex override: using fallback auth from " + authPath);
      return fallback;
    } catch (e) {
      logWarn(ctx, "codex override: failed to read fallback auth from " + authPath + ": " + String(e));
    }
  }

  return null;
}

function persistAuthToOpencode(ctx, authState) {
  if (!authState || authState.source !== "opencode" || !isNonEmptyString(authState.authPath)) {
    return false;
  }

  const auth = authState.auth;
  if (!auth || typeof auth !== "object") {
    return false;
  }

  const tokens = auth.tokens;
  if (!tokens || typeof tokens !== "object") {
    return false;
  }

  const fs = ctx && ctx.host && ctx.host.fs;
  if (!fs || typeof fs.readText !== "function" || typeof fs.writeText !== "function") {
    return false;
  }

  let doc = {};
  try {
    if (fs.exists(authState.authPath)) {
      doc = parseJsonLoose(fs.readText(authState.authPath)) || {};
    }
  } catch (_) {
    doc = {};
  }
  if (!doc || typeof doc !== "object") {
    doc = {};
  }

  let openai = doc[OPENAI_PROVIDER_KEY];
  if (!openai || typeof openai !== "object") {
    openai = {};
  }

  if (isNonEmptyString(tokens.access_token)) {
    openai.access = tokens.access_token;
  }
  if (isNonEmptyString(tokens.refresh_token)) {
    openai.refresh = tokens.refresh_token;
  }
  if (isNonEmptyString(tokens.account_id)) {
    openai.accountId = tokens.account_id;
  }
  if (!isNonEmptyString(openai.type)) {
    openai.type = "oauth";
  }

  doc[OPENAI_PROVIDER_KEY] = openai;
  fs.writeText(authState.authPath, JSON.stringify(doc, null, 2));
  logInfo(ctx, "codex override: persisted auth to " + authState.authPath);
  return true;
}
