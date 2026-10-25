import { beforeEach, describe, expect, it, vi } from "vitest"
import { makeCtx } from "../test-helpers.js"

const loadPlugin = async () => {
  await import("./plugin.js")
  return globalThis.__openusage_plugin
}

const COOKIES = {
  SID: "sid-value",
  SAPISID: "sapisid-value",
  "__Secure-1PSID": "psid-value",
  "__Secure-1PSIDTS": "psidts-value",
}

const USAGE_HTML = `
  <html>
    <script>window.WIZ_global_data = {"SNlM0e":"at-token","cfb2h":"boq_test","FdrFJe":"99"};</script>
    <div>Upgrade to Pro. Gemini 2.5 Pro. Try Ultra.</div>
  </html>
`

function planRpc(label) {
  return `)]}'
[["wrb.fr","sJBwce","[\\"${label}\\"]",null,null,null,"generic"]]
`
}

const PLAN_RPC_PRO = planRpc("Pro")
const PLAN_RPC_EMPTY = `)]}'
[["wrb.fr","sJBwce","[]",null,null,null,"generic"]]
`

const USAGE_RPC = `)]}'
[["wrb.fr","jSf9Qc","[2,[[35531,0.2656,2,[[1788254602]]],[2093,0.13,1,[[1788200602]]]]]",null,null,null,"generic"]]
`

const LIVE_RPC = `)]}'

202
[["wrb.fr","jSf9Qc","[2,[[1992,0.17,1,[[1788200602,190858000]]],[35430,0.26772428,2,[[1788254602,191017000]]]],false]",null,null,null,"generic"],["di",314],["af.httprm",314,"-7294595915490677503",64]]
25
[["e",4,null,null,238]]
`

function mockChromeCookies(ctx, map = COOKIES) {
  ctx.host.chromiumCookies.read.mockReturnValue(map)
}

function mockGeminiHttp(ctx, { html = USAGE_HTML, rpc = USAGE_RPC, plan = PLAN_RPC_PRO } = {}) {
  ctx.host.http.request.mockImplementation((opts) => {
    if (String(opts.url).startsWith("https://gemini.google.com/app") || String(opts.url).startsWith("https://gemini.google.com/usage")) {
      expect(opts.headers.Cookie).toContain("__Secure-1PSID=psid-value")
      expect(opts.headers["X-Same-Domain"]).toBe("1")
      expect(opts.http1Only).toBe(true)
      return { status: 200, bodyText: html }
    }
    if (String(opts.url).includes("batchexecute")) {
      expect(opts.method).toBe("POST")
      expect(opts.bodyText).toContain("f.req=")
      expect(opts.bodyText).toContain("at=at-token")
      if (String(opts.url).includes("rpcids=jSf9Qc")) {
        return { status: 200, bodyText: rpc }
      }
      if (String(opts.url).includes("rpcids=sJBwce")) {
        return { status: 200, bodyText: plan }
      }
      throw new Error("unexpected batchexecute " + opts.url)
    }
    throw new Error("unexpected url " + opts.url)
  })
}

describe("gemini-apps plugin", () => {
  beforeEach(() => {
    delete globalThis.__openusage_plugin
    vi.resetModules()
  })

  it("throws when no auth is available", async () => {
    const ctx = makeCtx()
    const plugin = await loadPlugin()
    expect(() => plugin.probe(ctx)).toThrow(/Log into gemini.google.com/)
  })

  it("maps jSf9Qc used fractions to Current and Weekly", async () => {
    const ctx = makeCtx()
    mockChromeCookies(ctx)
    mockGeminiHttp(ctx)
    const plugin = await loadPlugin()
    const result = plugin.probe(ctx)

    expect(result.plan).toBe("Pro")
    const current = result.lines.find((line) => line.label === "Current")
    const weekly = result.lines.find((line) => line.label === "Weekly")
    expect(current.used).toBe(13)
    expect(current.limit).toBe(100)
    expect(current.resetsAt).toBe(ctx.util.toIso(1788200602))
    expect(current.periodDurationMs).toBe(5 * 60 * 60 * 1000)
    expect(weekly.used).toBe(27)
    expect(weekly.limit).toBe(100)
    expect(weekly.resetsAt).toBe(ctx.util.toIso(1788254602))
    expect(weekly.periodDurationMs).toBe(7 * 24 * 60 * 60 * 1000)
    expect(result.lines.find((line) => line.label === "Source").value).toBe("Chrome")
  })

  it("parses length-prefixed batchexecute frames from Gemini", async () => {
    const ctx = makeCtx()
    mockChromeCookies(ctx)
    mockGeminiHttp(ctx, { rpc: LIVE_RPC })
    const plugin = await loadPlugin()
    const result = plugin.probe(ctx)
    expect(result.lines.find((line) => line.label === "Current").used).toBe(17)
    expect(result.lines.find((line) => line.label === "Weekly").used).toBe(27)
  })

  it("does not invert used fractions like Antigravity remaining", async () => {
    const ctx = makeCtx()
    mockChromeCookies(ctx)
    mockGeminiHttp(ctx)
    const plugin = await loadPlugin()
    const result = plugin.probe(ctx)
    const current = result.lines.find((line) => line.label === "Current")
    expect(current.used).toBe(13)
    expect(current.used).not.toBe(87)
  })

  it("reads GEMINI_COOKIE when Chrome cookies are missing", async () => {
    const ctx = makeCtx()
    ctx.host.env.get.mockImplementation((name) =>
      name === "GEMINI_COOKIE"
        ? "SID=sid-value; SAPISID=sapisid-value; __Secure-1PSID=psid-value"
        : null,
    )
    mockGeminiHttp(ctx)
    const plugin = await loadPlugin()
    const result = plugin.probe(ctx)
    expect(result.lines.find((line) => line.label === "Current").used).toBe(13)
    expect(result.lines.find((line) => line.label === "Source").value).toBe("GEMINI_COOKIE")
  })

  it("throws when the usage page is a login redirect", async () => {
    const ctx = makeCtx()
    mockChromeCookies(ctx)
    ctx.host.http.request.mockImplementation(() => ({
      status: 302,
      bodyText: "https://accounts.google.com/ServiceLogin",
    }))
    const plugin = await loadPlugin()
    expect(() => plugin.probe(ctx)).toThrow(/session expired/)
  })

  it("prefers sJBwce over HTML upsell copy for the plan label", async () => {
    const ctx = makeCtx()
    mockChromeCookies(ctx)
    mockGeminiHttp(ctx, { plan: planRpc("Free") })
    const plugin = await loadPlugin()
    const result = plugin.probe(ctx)
    expect(result.plan).toBe("Free")
  })

  it("does not treat HTML Pro/Ultra marketing text as a plan", async () => {
    const ctx = makeCtx()
    mockChromeCookies(ctx)
    mockGeminiHttp(ctx, { plan: PLAN_RPC_EMPTY })
    const plugin = await loadPlugin()
    const result = plugin.probe(ctx)
    expect(result.plan).toBeUndefined()
  })

  it("uses Ultra from sJBwce even when HTML mentions Pro first", async () => {
    const ctx = makeCtx()
    mockChromeCookies(ctx)
    mockGeminiHttp(ctx, { plan: planRpc("Ultra") })
    const plugin = await loadPlugin()
    const result = plugin.probe(ctx)
    expect(result.plan).toBe("Ultra")
  })

  it("falls back to GEMINI_COOKIE when Chrome cookies are only a partial session", async () => {
    const ctx = makeCtx()
    mockChromeCookies(ctx, { SID: "sid-only" })
    ctx.host.env.get.mockImplementation((name) =>
      name === "GEMINI_COOKIE"
        ? "SAPISID=sapisid-value; __Secure-1PSID=psid-value"
        : null,
    )
    mockGeminiHttp(ctx)
    const plugin = await loadPlugin()
    const result = plugin.probe(ctx)
    expect(result.lines.find((line) => line.label === "Source").value).toBe("GEMINI_COOKIE")
  })

  it("falls back to GEMINI_COOKIE when Chrome has PSID but no SAPISID", async () => {
    const ctx = makeCtx()
    mockChromeCookies(ctx, { SID: "sid-value", "__Secure-1PSID": "stale-psid" })
    ctx.host.env.get.mockImplementation((name) =>
      name === "GEMINI_COOKIE"
        ? "SAPISID=sapisid-value; __Secure-1PSID=psid-value"
        : null,
    )
    mockGeminiHttp(ctx)
    const plugin = await loadPlugin()
    const result = plugin.probe(ctx)
    expect(result.lines.find((line) => line.label === "Source").value).toBe("GEMINI_COOKIE")
  })
})
