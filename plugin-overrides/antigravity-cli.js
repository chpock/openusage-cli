(function () {
  globalThis.__openusage_override.wrapProbe(function (ctx, previousProbe) {
    var keychain = ctx.host.keychain;
    if (!keychain || typeof keychain.readGenericPassword !== "function") {
      return previousProbe(ctx);
    }

    var serviceOnly = false;
    var scopedKeychain = Object.create(keychain);
    scopedKeychain.readGenericPassword = function (service, account) {
      var relevant = service === "gemini" && (account === undefined || account === "antigravity");
      if (relevant) serviceOnly = false;
      var value = keychain.readGenericPassword.apply(keychain, arguments);
      if (relevant && value) serviceOnly = account === undefined;
      return value;
    };
    if (typeof keychain.writeGenericPasswordForAccount === "function") {
      scopedKeychain.writeGenericPasswordForAccount = function (service, account, value) {
        // The native refresh writer hard-codes an account even after a service-only read.
        if (serviceOnly && service === "gemini" && account === "antigravity") {
          return keychain.writeGenericPassword(service, value);
        }
        return keychain.writeGenericPasswordForAccount.apply(keychain, arguments);
      };
    }

    var scopedHost = Object.create(ctx.host);
    scopedHost.keychain = scopedKeychain;
    var scopedCtx = Object.create(ctx);
    scopedCtx.host = scopedHost;
    return previousProbe(scopedCtx);
  });
})();
