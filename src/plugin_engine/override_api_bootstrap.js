(function(pluginId) {
    var plugin = globalThis.__openusage_plugin;
    if (!plugin || typeof plugin !== "object") {
        throw "missing __openusage_plugin before override init";
    }
    if (typeof plugin.probe !== "function") {
        throw "missing probe() before override init";
    }

    var originalProbe = plugin.probe.bind(plugin);
    var originalDiscoverAccounts = typeof plugin.discoverAccounts === "function"
        ? plugin.discoverAccounts.bind(plugin)
        : null;

    globalThis.__openusage_override = {
        pluginId: pluginId,
        originalProbe: originalProbe,
        replaceProbe: function(replacement) {
            if (typeof replacement !== "function") {
                throw "replaceProbe expects a function";
            }
            plugin.probe = function(ctx) {
                return replacement(ctx, originalProbe);
            };
            return plugin.probe;
        },
        wrapProbe: function(wrapper) {
            if (typeof wrapper !== "function") {
                throw "wrapProbe expects a function";
            }
            var previousProbe = plugin.probe.bind(plugin);
            plugin.probe = function(ctx) {
                return wrapper(ctx, previousProbe, originalProbe);
            };
            return plugin.probe;
        },
        resetProbe: function() {
            plugin.probe = originalProbe;
            return plugin.probe;
        },
        originalDiscoverAccounts: originalDiscoverAccounts,
        replaceDiscoverAccounts: function(replacement) {
            if (typeof replacement !== "function") {
                throw "replaceDiscoverAccounts expects a function";
            }
            plugin.discoverAccounts = function(ctx) {
                return replacement(ctx, originalDiscoverAccounts);
            };
            return plugin.discoverAccounts;
        },
        wrapDiscoverAccounts: function(wrapper) {
            if (typeof wrapper !== "function") {
                throw "wrapDiscoverAccounts expects a function";
            }
            var currentDiscoverAccounts = typeof plugin.discoverAccounts === "function"
                ? plugin.discoverAccounts.bind(plugin)
                : null;
            if (!currentDiscoverAccounts) {
                throw "wrapDiscoverAccounts requires a current discoverAccounts function";
            }
            plugin.discoverAccounts = function(ctx) {
                return wrapper(ctx, currentDiscoverAccounts, originalDiscoverAccounts);
            };
            return plugin.discoverAccounts;
        },
        resetDiscoverAccounts: function() {
            if (originalDiscoverAccounts) {
                plugin.discoverAccounts = originalDiscoverAccounts;
            } else {
                delete plugin.discoverAccounts;
            }
            return typeof plugin.discoverAccounts === "function"
                ? plugin.discoverAccounts
                : null;
        }
    };
})