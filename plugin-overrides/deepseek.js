globalThis.__openusage_ast_patch = {
  functions: [{ target: "findBalance", with: "patchFindBalance", mode: "wrap" }],
};

var patchFindBalance = (function () {
  var selectedCurrency = null;

  function validNumber(value) {
    if (typeof value === "number") return Number.isFinite(value);
    return typeof value === "string" && value.trim() !== "" && Number.isFinite(Number(value));
  }

  globalThis.__openusage_override.wrapProbe(function (ctx, previousProbe) {
    selectedCurrency = null;
    try {
      var result = previousProbe(ctx);
      if (selectedCurrency === "CNY") {
        for (var i = 0; i < result.lines.length; i += 1) {
          var line = result.lines[i];
          if (line.type === "progress" && line.label === "Balance") {
            line.format = { kind: "currency", currency: "CNY" };
          }
        }
      }
      return result;
    } finally {
      selectedCurrency = null;
    }
  });

  return function (original, balanceInfos) {
    var balance = original(balanceInfos);
    if (balance === null) return balance;
    for (var i = 0; i < balanceInfos.length; i += 1) {
      var info = balanceInfos[i];
      if (!info || typeof info !== "object" || !validNumber(info.total_balance)) continue;
      if (info.currency === "USD") {
        selectedCurrency = "USD";
        return balance;
      }
      if (info.currency === "CNY") selectedCurrency = "CNY";
    }
    return balance;
  };
})();
