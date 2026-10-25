globalThis.__openusage_ast_patch = {
  functions: [
    { target: "creditsLines", with: "patchCreditsLines", mode: "wrap" },
    { target: "appendSpend", with: "patchAppendSpend", mode: "wrap" },
  ],
};

var patchCreditsLines = (function () {
  function readNumber(value) {
    if (typeof value === "number") return Number.isFinite(value) ? value : null;
    if (typeof value !== "string" || !value.trim()) return null;
    var number = Number(value);
    return Number.isFinite(number) ? number : null;
  }

  function usdText(ctx, amount) {
    var cents = amount * 100;
    // Numbers this large cannot carry fractional-dollar precision.
    return "$" + String(Number.isFinite(cents) ? ctx.fmt.dollars(cents) : amount);
  }

  globalThis.patchAppendSpend = function (original, ctx, value, label, lines) {
    var before = lines.length;
    original(ctx, value, label, lines);
    if (lines.length > before) {
      lines[before].value = usdText(ctx, Math.max(0, readNumber(value)));
    }
  };

  return function (original, ctx, data) {
    var lines = original(ctx, data);
    for (var i = 0; i < lines.length; i += 1) {
      if (lines[i].type === "text" && lines[i].label === "Balance") {
        var used = Math.max(0, readNumber(data.total_usage));
        var credits = Math.max(0, readNumber(data.total_credits) || 0);
        lines[i].value = usdText(ctx, Math.max(0, credits - used));
      }
    }
    return lines;
  };
})();

var patchAppendSpend = (function () {
  return globalThis.patchAppendSpend;
})();
