(function () {
  const APP_URL = "https://gemini.google.com/app"
  const USAGE_URL = "https://gemini.google.com/usage"
  const BATCH_URL = "https://gemini.google.com/_/BardChatUi/data/batchexecute"
  const ORIGIN = "https://gemini.google.com"
  const RPC_USAGE = "jSf9Qc"
  const RPC_PLAN = "sJBwce"
  const CURRENT_MS = 5 * 60 * 60 * 1000
  const WEEKLY_MS = 7 * 24 * 60 * 60 * 1000
  const BRAND = "#3186FF"
  const COOKIE_HOSTS = [".google.com", "google.com", ".gemini.google.com", "gemini.google.com"]
  const COOKIE_NAMES = [
    "SID",
    "HSID",
    "SSID",
    "APISID",
    "SAPISID",
    "__Secure-1PSID",
    "__Secure-1PSIDTS",
    "__Secure-3PSID",
    "__Secure-1PAPISID",
  ]

  function readString(value) {
    if (typeof value !== "string") return null
    const trimmed = value.trim()
    return trimmed ? trimmed : null
  }

  function getEnv(ctx, name) {
    try {
      return readString(ctx.host.env.get(name))
    } catch (e) {
      ctx.host.log.warn("env read failed for " + name + ": " + String(e))
      return null
    }
  }

  function parseCookieHeader(raw) {
    const text = readString(raw)
    if (!text) return null
    const header = text.replace(/^Cookie:\s*/i, "")
    const map = {}
    const parts = header.split(";")
    for (let i = 0; i < parts.length; i += 1) {
      const part = parts[i].trim()
      const eq = part.indexOf("=")
      if (eq === -1) continue
      const name = part.slice(0, eq).trim()
      const value = part.slice(eq + 1).trim()
      if (name && value) map[name] = value
    }
    return Object.keys(map).length > 0 ? map : null
  }

  function hasGeminiSession(map) {
    if (!map || typeof map !== "object") return false
    const hasPsid = readString(map["__Secure-1PSID"])
    const hasSapisid = readString(map.SAPISID) || readString(map["__Secure-1PAPISID"])
    return !!(hasPsid && hasSapisid)
  }

  function readChromeCookies(ctx) {
    if (!ctx.host.chromiumCookies || typeof ctx.host.chromiumCookies.read !== "function") {
      return null
    }
    try {
      const map = ctx.host.chromiumCookies.read({ hosts: COOKIE_HOSTS, names: COOKIE_NAMES })
      if (!hasGeminiSession(map)) return null
      return map
    } catch (e) {
      ctx.host.log.warn("chromium cookie read failed: " + String(e))
      return null
    }
  }

  function loadCookies(ctx) {
    const chrome = readChromeCookies(ctx)
    if (chrome) return { map: chrome, source: "Chrome" }

    const envMap = parseCookieHeader(getEnv(ctx, "GEMINI_COOKIE"))
    if (envMap) return { map: envMap, source: "GEMINI_COOKIE" }

    return null
  }

  function headerSafe(value) {
    const text = readString(value)
    if (!text) return null
    for (let i = 0; i < text.length; i += 1) {
      const code = text.charCodeAt(i)
      if (code < 32 || code > 126) return null
    }
    return text
  }

  function cookieHeader(map) {
    const parts = []
    for (let i = 0; i < COOKIE_NAMES.length; i += 1) {
      const name = COOKIE_NAMES[i]
      const value = headerSafe(map[name])
      if (value) parts.push(name + "=" + value)
    }
    const keys = Object.keys(map)
    for (let i = 0; i < keys.length; i += 1) {
      const name = keys[i]
      if (COOKIE_NAMES.indexOf(name) !== -1) continue
      const value = headerSafe(map[name])
      if (value) parts.push(name + "=" + value)
    }
    return parts.join("; ")
  }

  function sapisidHash(ctx, map) {
    const sapisid = readString(map.SAPISID) || readString(map["__Secure-1PAPISID"])
    if (!sapisid) return null
    if (!ctx.host.crypto || typeof ctx.host.crypto.sha1Hex !== "function") {
      ctx.host.log.warn("sha1Hex missing; SAPISIDHASH skipped")
      return null
    }
    const ts = Math.floor(Date.parse(ctx.nowIso || "") / 1000)
    const timestamp = Number.isFinite(ts) && ts > 0 ? ts : Math.floor(Date.now() / 1000)
    const digest = ctx.host.crypto.sha1Hex(timestamp + " " + sapisid + " " + ORIGIN)
    return "SAPISIDHASH " + timestamp + "_" + digest
  }

  function geminiHeaders(map, extra) {
    const headers = {
      Cookie: cookieHeader(map),
      Origin: ORIGIN,
      Referer: USAGE_URL,
      "X-Same-Domain": "1",
      "User-Agent":
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/144.0.0.0 Safari/537.36",
    }
    if (extra) {
      const keys = Object.keys(extra)
      for (let i = 0; i < keys.length; i += 1) headers[keys[i]] = extra[keys[i]]
    }
    return headers
  }

  function extractMeta(html) {
    function field(name) {
      const re = new RegExp('"' + name + '"\\s*:\\s*"([^"]+)"')
      const m = re.exec(html)
      if (m && m[1]) return m[1]
      const re2 = new RegExp("'" + name + "'\\s*:\\s*'([^']+)'")
      const m2 = re2.exec(html)
      return m2 && m2[1] ? m2[1] : null
    }
    return {
      at: field("SNlM0e"),
      bl: field("cfb2h"),
      sid: field("FdrFJe"),
    }
  }

  function looksLikeLogin(html, status) {
    if (status === 302 || status === 303 || status === 307 || status === 308) return true
    const text = String(html || "")
    if (/accounts\.google\.com\/(?:ServiceLogin|v3\/signin)/i.test(text) && !/"SNlM0e"/.test(text)) {
      return true
    }
    return false
  }

  function parsePlan(text) {
    const s = String(text || "")
    if (/\bUltra\b/i.test(s)) return "Ultra"
    if (/\bPRO\b/.test(s) || /\bPro\b/.test(s)) return "Pro"
    if (/\bFree\b/i.test(s)) return "Free"
    return null
  }

  function tryParseJson(ctx, text) {
    return ctx.util.tryParseJson(text)
  }

  function parseBatchexecuteFrames(ctx, text) {
    let s = String(text || "").replace(/^\)\]\}'\s*/, "")
    const frames = []
    let pos = 0
    while (pos < s.length) {
      while (pos < s.length && /\s/.test(s.charAt(pos))) pos += 1
      if (pos >= s.length) break
      if (s.charAt(pos) === "[") {
        const parsed = tryParseJson(ctx, s.slice(pos))
        if (parsed) frames.push(parsed)
        break
      }
      const nl = s.indexOf("\n", pos)
      if (nl < 0) break
      const n = Number(s.slice(pos, nl).trim())
      if (!Number.isFinite(n) || n <= 0) {
        pos = nl + 1
        continue
      }
      const frame = s.slice(nl + 1, nl + 1 + n)
      const parsed = tryParseJson(ctx, frame)
      if (parsed) frames.push(parsed)
      pos = nl + 1 + n
    }
    return frames
  }

  function extractRpcPayload(ctx, text, rpcId) {
    const marker = '"' + rpcId + '",'
    const idx = String(text || "").indexOf(marker)
    if (idx === -1) return findWrbPayload(ctx, parseBatchexecuteFrames(ctx, text), rpcId)
    let p = idx + marker.length
    while (p < text.length && /\s/.test(text.charAt(p))) p += 1
    if (text.charAt(p) !== '"') {
      return findWrbPayload(ctx, parseBatchexecuteFrames(ctx, text), rpcId)
    }
    let i = p + 1
    while (i < text.length) {
      const ch = text.charAt(i)
      if (ch === "\\") {
        i += 2
        continue
      }
      if (ch === '"') {
        const sliced = text.slice(p, i + 1)
        const inner = tryParseJson(ctx, sliced)
        if (typeof inner === "string") return tryParseJson(ctx, inner) || inner
        return inner
      }
      i += 1
    }
    return findWrbPayload(ctx, parseBatchexecuteFrames(ctx, text), rpcId)
  }

  function findWrbPayload(ctx, frames, rpcId) {
    function walk(node) {
      if (!node) return null
      if (Array.isArray(node)) {
        if (node[0] === "wrb.fr" && node[1] === rpcId) {
          const payload = node[2]
          if (typeof payload === "string") return tryParseJson(ctx, payload) || payload
          return payload
        }
        for (let i = 0; i < node.length; i += 1) {
          const found = walk(node[i])
          if (found != null) return found
        }
      }
      return null
    }
    for (let i = 0; i < frames.length; i += 1) {
      const found = walk(frames[i])
      if (found != null) return found
    }
    return null
  }

  function resetUnix(row) {
    if (!row || !Array.isArray(row[3])) return null
    const first = row[3][0]
    if (Array.isArray(first)) {
      const n = Number(first[0])
      return Number.isFinite(n) && n > 0 ? n : null
    }
    const n = Number(first)
    return Number.isFinite(n) && n > 0 ? n : null
  }

  function parseUsagePayload(payload) {
    let rows = null
    if (Array.isArray(payload) && Array.isArray(payload[1])) rows = payload[1]
    else if (Array.isArray(payload) && payload.length && Array.isArray(payload[0])) rows = payload
    if (!rows) return null

    let current = null
    let weekly = null
    for (let i = 0; i < rows.length; i += 1) {
      const row = rows[i]
      if (!Array.isArray(row) || row.length < 3) continue
      const frac = Number(row[1])
      const type = Number(row[2])
      if (!Number.isFinite(frac) || !Number.isFinite(type)) continue
      const used = Math.max(0, Math.min(100, Math.round(frac * 100)))
      const bucket = { used: used, resetsAtUnix: resetUnix(row) }
      if (type === 1) current = bucket
      else if (type === 2) weekly = bucket
    }
    if (!current || !weekly) return null
    return { current: current, weekly: weekly }
  }

  function encodeForm(fields) {
    const parts = []
    const keys = Object.keys(fields)
    for (let i = 0; i < keys.length; i += 1) {
      const key = keys[i]
      if (fields[key] == null) continue
      parts.push(encodeURIComponent(key) + "=" + encodeURIComponent(String(fields[key])))
    }
    return parts.join("&")
  }

  function batchExecute(ctx, cookies, meta, rpcId, innerPayload) {
    const params = [
      "rpcids=" + encodeURIComponent(rpcId),
      "rt=c",
      "source-path=" + encodeURIComponent("/usage"),
    ]
    if (meta.bl) params.push("bl=" + encodeURIComponent(meta.bl))
    if (meta.sid) params.push("f.sid=" + encodeURIComponent(meta.sid))
    params.push("_reqid=" + String(100000 + Math.floor(Math.random() * 900000)))

    const headers = geminiHeaders(cookies.map, {
      "Content-Type": "application/x-www-form-urlencoded;charset=utf-8",
    })
    const auth = sapisidHash(ctx, cookies.map)
    if (auth) headers.Authorization = auth

    const form = encodeForm({
      "f.req": JSON.stringify([[[rpcId, innerPayload, null, "generic"]]]),
      at: meta.at,
    })

    let resp
    try {
      resp = ctx.util.request({
        method: "POST",
        url: BATCH_URL + "?" + params.join("&"),
        headers: headers,
        bodyText: form,
        timeoutMs: 15000,
        http1Only: true,
      })
    } catch (e) {
      ctx.host.log.warn("batchexecute failed (" + rpcId + "): " + String(e))
      return null
    }
    if (looksLikeLogin(resp.bodyText, resp.status)) {
      throw "Gemini Apps session expired. Log into gemini.google.com in Chrome."
    }
    if (resp.status < 200 || resp.status >= 300) {
      ctx.host.log.warn("batchexecute HTTP " + resp.status + " for " + rpcId)
      return null
    }
    return extractRpcPayload(ctx, resp.bodyText, rpcId)
  }

  function fetchHtml(ctx, cookies, url) {
    const headers = geminiHeaders(cookies.map, { Accept: "text/html" })
    const auth = sapisidHash(ctx, cookies.map)
    if (auth) headers.Authorization = auth
    let resp
    try {
      resp = ctx.util.request({
        method: "GET",
        url: url,
        headers: headers,
        timeoutMs: 15000,
        http1Only: true,
      })
    } catch (e) {
      ctx.host.log.warn("html request failed (" + url + "): " + String(e))
      throw "Could not reach gemini.google.com. Check your connection."
    }
    if (looksLikeLogin(resp.bodyText, resp.status)) {
      throw "Gemini Apps session expired. Log into gemini.google.com in Chrome."
    }
    if (ctx.util.isAuthStatus(resp.status)) {
      throw "Gemini Apps session expired. Log into gemini.google.com in Chrome."
    }
    if (resp.status < 200 || resp.status >= 300) {
      throw "Gemini Apps page failed (HTTP " + resp.status + ")."
    }
    return resp.bodyText
  }

  function fetchUsagePage(ctx, cookies) {
    let html = fetchHtml(ctx, cookies, APP_URL)
    let meta = extractMeta(html)
    if (!meta.at) {
      ctx.host.log.info("SNlM0e missing on /app; trying /usage")
      html = fetchHtml(ctx, cookies, USAGE_URL)
      meta = extractMeta(html)
    }
    ctx.host.log.info(
      "htmlChars=" +
        String(html.length) +
        " hasAt=" +
        (meta.at ? "yes" : "no") +
        " hasBl=" +
        (meta.bl ? "yes" : "no"),
    )
    if (!meta.at) {
      throw "Could not read Gemini Apps session token. Log into gemini.google.com in Chrome."
    }
    return { html: html, meta: meta }
  }

  function probe(ctx) {
    const cookies = loadCookies(ctx)
    if (!cookies) {
      throw "Gemini Apps auth missing. Log into gemini.google.com in Chrome."
    }
    ctx.host.log.info("cookie names=" + Object.keys(cookies.map).sort().join(","))
    if (!hasGeminiSession(cookies.map)) {
      throw "Gemini Apps auth missing. Log into gemini.google.com in Chrome."
    }

    const page = fetchUsagePage(ctx, cookies)
    const payload = batchExecute(ctx, cookies, page.meta, RPC_USAGE, "[]")
    const usage = parseUsagePayload(payload)
    if (!usage) throw "Could not parse Gemini Apps usage."

    let plan = null
    try {
      const planPayload = batchExecute(ctx, cookies, page.meta, RPC_PLAN, "[]")
      if (planPayload != null) {
        plan = parsePlan(JSON.stringify(planPayload))
      }
    } catch (e) {
      ctx.host.log.warn("plan RPC skipped: " + String(e))
    }

    const currentResetsAt = usage.current.resetsAtUnix
      ? ctx.util.toIso(usage.current.resetsAtUnix)
      : undefined
    const weeklyResetsAt = usage.weekly.resetsAtUnix
      ? ctx.util.toIso(usage.weekly.resetsAtUnix)
      : undefined

    const lines = [
      ctx.line.progress({
        label: "Current",
        used: usage.current.used,
        limit: 100,
        format: { kind: "percent" },
        resetsAt: currentResetsAt,
        periodDurationMs: CURRENT_MS,
        color: BRAND,
      }),
      ctx.line.progress({
        label: "Weekly",
        used: usage.weekly.used,
        limit: 100,
        format: { kind: "percent" },
        resetsAt: weeklyResetsAt,
        periodDurationMs: WEEKLY_MS,
        color: BRAND,
      }),
      ctx.line.text({ label: "Source", value: cookies.source }),
    ]

    const result = { lines: lines }
    if (plan) result.plan = ctx.fmt.planLabel(plan)
    return result
  }

  globalThis.__openusage_plugin = {
    id: "gemini-apps",
    probe: probe,
  }
})()
