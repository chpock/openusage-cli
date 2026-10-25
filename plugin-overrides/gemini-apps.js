globalThis.__openusage_ast_patch = {
  functions: [{ target: "readChromeCookies", with: "patchReadChromeCookies", mode: "wrap" }],
};

var patchReadChromeCookies = (function () {
  return function (original, ctx) {
    var reader = ctx.host.chromiumCookies;
    if (!reader || typeof reader.read !== "function" || typeof reader.readProfiles !== "function") {
      return original(ctx);
    }

    var profiles = null;
    var index = 0;
    var scopedReader = Object.create(reader);
    scopedReader.read = function (opts) {
      // Keep the native options, acceptance predicate and read-error handling authoritative.
      if (profiles === null) profiles = reader.readProfiles(opts);
      return profiles[index];
    };
    var scopedHost = Object.create(ctx.host);
    scopedHost.chromiumCookies = scopedReader;
    var scopedCtx = Object.create(ctx);
    scopedCtx.host = scopedHost;

    do {
      var map = original(scopedCtx);
      if (map) return map;
      index += 1;
    } while (profiles && index < profiles.length);
    return null;
  };
})();
