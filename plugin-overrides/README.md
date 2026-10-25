# Bundled plugin overrides

These first-party scripts extend or correct the matching CrossUsage plugins
without editing `vendor/crossusage/`. Some add permanent functionality; others
are temporary repairs for verified upstream defects.

For loading, file naming and the override API, see
[Plugin Overrides](../docs/plugin-overrides.md). The
[workaround register](../docs/upstream-plugin-workarounds.md) records known-bad
upstream revisions, exact repair scope, regressions and removal conditions.

## [codex.js](codex.js)

- Adds OpenCode multi-account discovery alongside the native `default` account,
  reading `auth.json` and optional `accounts.json` in
  `~/.local/share/opencode/` and `~/.config/opencode/`.
- Keeps native credential selection for `default`; selected OpenCode accounts
  never fall back to native credentials or another OpenCode source. Reloads and
  refreshes credentials from the selected source and persists refreshed tokens
  back to that source, rejecting detected account conflicts or persistence failure.
- Subscribes to both OpenCode files in each directory so changes trigger a
  provider refresh in the daemon.
- **CU-003:** makes optional reset-credit detail requests use the token/account
  pair from the successful usage request, including after token refresh.

Details: [multi-account discovery](../docs/usage.md#multi-account-discovery),
[authentication-source selection](../docs/configuration.md#authentication-sources),
[CU-003 and its removable scope](../docs/upstream-plugin-workarounds.md#cu-003-codex-reset-detail-credentials).
The account-routing and monitoring features are not temporary workarounds.

## [copilot.js](copilot.js)

- Adds OpenCode multi-account discovery for `github-copilot` credentials from the
  same auth/accounts file locations as Codex, alongside native `default` loading.
- Uses `auth.json` for the active OpenCode account and `accounts.json` data for
  inactive accounts when an account list is present. Each selected OpenCode
  account uses its own token without native or cross-file fallback.
- Subscribes to the four candidate files for daemon refresh. OpenCode routing
  does not add credential writes; default-account credential loading and writes
  are delegated to the native plugin.

Details: [multi-account discovery](../docs/usage.md#multi-account-discovery) and
[authentication-source selection](../docs/configuration.md#authentication-sources).
This is a first-party feature, not a temporary upstream-bug repair.

## [openrouter.js](openrouter.js)

**CU-001:** corrects USD balance and daily/weekly/monthly spend text. Upstream
passes dollar amounts to a formatter that expects cents, understating the text
values. The override fixes only those formatter inputs; progress amounts, shared
formatter semantics, credential selection and requests remain unchanged.

Details: [CU-001](../docs/upstream-plugin-workarounds.md#cu-001-openrouter-usd-text).

## [deepseek.js](deepseek.js)

**CU-002:** preserves the currency of a CNY-only balance instead of labelling it
USD. USD remains preferred when available; CNY uses the explicit `currency`
format. No exchange-rate conversion or amount rescaling is performed.
`DEEPSEEK_INITIAL_BALANCE` must use the selected balance's currency.

Details: [CU-002](../docs/upstream-plugin-workarounds.md#cu-002-deepseek-balance-currency)
and [output units](../docs/json-output.md#progress-formats-and-units).

## [antigravity-cli.js](antigravity-cli.js)

**CU-004:** when native authentication falls back from an explicit keychain
account to a service-only lookup, refresh writes back through that successful
selector rather than the hard-coded account. Explicit-account success is
unchanged; the correction is scoped to this probe and does not alter the shared
keychain API or retarget independent writes.

Details: [CU-004](../docs/upstream-plugin-workarounds.md#cu-004-antigravity-cli-refresh-destination).

## [gemini-apps.js](gemini-apps.js)

**CU-005:** tests separate Chromium profile cookie maps with Gemini's native
session predicate and chooses the first usable profile. This prevents a profile
with more optional cookies but no valid session from hiding a usable profile.
Uses `host.chromiumCookies.readProfiles` without changing the generic `read`
selection; cookies are never combined across profiles. Native environment
fallback and request/parsing behavior remain unchanged.

Details: [CU-005](../docs/upstream-plugin-workarounds.md#cu-005-gemini-apps-cookie-profile-selection)
and [Chromium source boundaries](../docs/crossusage_differences.md#chromium-cookie-sources).

## Maintenance

Before removing a temporary correction, follow the register's
[recheck and retirement procedure](../docs/upstream-plugin-workarounds.md#recheck-and-retirement-procedure):
prove the candidate native fix with only that correction disabled, retain its
regression tests, and preserve unrelated features. In particular, an upstream fix
for CU-003 does not justify deleting the whole Codex override.

Relative documentation links refer to the source checkout. For an installed
package copy, use the [repository documentation](https://github.com/chpock/openusage-cli/tree/main/docs).
