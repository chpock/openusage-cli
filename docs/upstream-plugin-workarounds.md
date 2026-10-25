# Upstream plugin workarounds

This register tracks temporary corrections to defects in the CrossUsage plugins,
not all behavior implemented by our overrides. Its purpose is to let an updater
prove that a native correction makes a particular workaround unnecessary and
remove only that workaround without losing unrelated functionality.

The verified defective snapshot for every entry below is **CrossUsage v1.6.1,
commit `ba1ad530b2148ba4be738332cb102858e9bdcc55`**. See the
[current vendor record](crossusage-upstream.md) and
[update evidence](crossusage-updates/v1.6.1.md). This identifies a known-bad
snapshot, not the first introducing commit or every affected release. No upstream
issue, fixing commit or fixed release has been verified for these entries.
Never infer a fix from a newer version number, release notes or a renamed function.

Vendored code stays unchanged. Local corrections live in `plugin-overrides/`;
the shared host APIs retain their documented contracts.

## Register

| ID | Native defect | Local correction | Status / retirement scope |
| --- | --- | --- | --- |
| [CU-001](#cu-001-openrouter-usd-text) | OpenRouter formats USD amounts as cents. | `openrouter.js`: `creditsLines` and `appendSpend` wrappers. | Implemented, regression-tested and independently reviewed. The file is currently correction-only. |
| [CU-002](#cu-002-deepseek-balance-currency) | DeepSeek accepts a CNY balance but declares USD output. | `deepseek.js`: currency selection tracking and probe wrapper. | Implemented, regression-tested and independently reviewed. The file is currently correction-only. |
| [CU-003](#cu-003-codex-reset-detail-credentials) | Codex's optional reset-details request can use the pre-refresh token. | Three targeted wrappers inside `codex.js`. | Implemented, regression-tested and independently reviewed. Do **not** remove the whole Codex override. |
| [CU-004](#cu-004-antigravity-cli-refresh-destination) | Antigravity CLI can read one keychain selector and refresh into another. | Caller-local `wrapProbe` correction in `antigravity-cli.js`. | Implemented, regression-tested and independently reviewed. The file is currently correction-only. |
| [CU-005](#cu-005-gemini-apps-cookie-profile-selection) | The browser reader can prefer more optional cookies over a valid Gemini session. | `gemini-apps.js`: `readChromeCookies` wrapper consuming separate `readProfiles` results. | Implemented, regression-tested and independently reviewed. The file is currently correction-only. |

An implemented correction is not proof of compatibility for the entire provider
or vendor update. Planned paths and checks are not completed validation.

For the complete monetary suite, including shared failure controls, run:

```sh
cargo test --locked --test provider_metrics_override
```

## Recheck and retirement procedure

For each update, follow [the override audit](crossusage-update-workflow.md#7-analyze-our-overrides)
and process every active entry:

1. Record the candidate upstream commit. Read its actual affected functions,
   callers and output, comparing them with the known-bad evidence below. A
   missing AST target may mean refactoring, not a corrected defect.
2. In an isolated candidate checkout, run the relevant behavioral fixtures with
   **this correction disabled**. Keep unrelated override features enabled. For
   a correction-only file, an empty temporary override lets existing fixtures
   that copy that file execute native behavior. For CU-003, disable only the
   three listed AST registrations, not account discovery/routing.
3. Exercise the bug-triggering case and the unaffected controls. Observe exact
   money/currency or request/write identities, not just successful HTTP status
   or absence of an exception. Green tests **with** the correction still active
   cannot establish an upstream fix.
4. If the defect persists, retain or adapt the correction and update its verified
   snapshot, source evidence and tests. If it is fixed, satisfy the entry's
   removal conditions and review the proposed excision. A changed native schema
   may require fixture adaptation grounded in the candidate contract; do not
   weaken the protected invariant merely to obtain a pass.
5. Remove only obsolete correction code and registrations. A fixture that
   unconditionally copies a retired override file must then allow its deliberate
   absence while retaining the same semantic assertions against native behavior.
   Do not delete the regression just because the workaround has gone.
6. Run affected provider, shared-host and public-output tests and `make ci-compact`,
   plus the update workflow's integration gates. Recheck that vendor bytes remain
   exact. Record independent review and any remaining validation gaps.
7. Keep the entry as a historical record: add the fixing or first verified-good
   upstream commit, the local removal commit when available, exact checks/results
   and disposition. Do not claim the first fixed release unless established.

### CU-001: OpenRouter USD text

**Known-bad evidence:** pinned
[`creditsLines` and `appendSpend`](https://github.com/barramee27/crossusage/blob/ba1ad530b2148ba4be738332cb102858e9bdcc55/plugins/openrouter/plugin.js#L97-L131).
The provider's [current-key schema](https://openrouter.ai/docs/api/api-reference/api-keys/get-current-key)
identifies `usage_daily`, `usage_weekly`, `usage_monthly` and limits as USD.
The [credits endpoint](https://openrouter.ai/docs/api/api-reference/credits/get-credits)
returns purchased and used credits in the same monetary scale. Provider docs
corroborate units; the pinned code and deterministic fixtures establish the defect.

**Cause and effect:** native text builders pass dollar amounts directly to
`ctx.fmt.dollars(cents)`, which divides by 100. Thus purchased credits `10` minus
usage `2.5` should produce balance `$7.5`, but native formatting produces `$0.08`.
Daily usage `1.25` becomes `$0.01` instead of `$1.25`. The progress lines already
carry USD major-unit values and must not be multiplied by 100.

**Local scope:** [openrouter.js](../plugin-overrides/openrouter.js) wraps
`creditsLines` with `patchCreditsLines` and `appendSpend` with `patchAppendSpend`.
It converts only their text formatter inputs to cents, retaining native request,
credential precedence, clamping, partial-response and error behavior. The
large-number branch avoids overflowing during multiplication. It does not change
the shared formatter or rescale progress values.

**Regression evidence:** [provider_metrics_override.rs](../tests/provider_metrics_override.rs):

- `openrouter_override_preserves_usd_units_and_native_requests` checks exact
  balance/daily/weekly/monthly text, progress amounts, URLs, bearer key and timeout.
- `openrouter_override_keeps_source_priority_clamping_and_partial_data` checks
  config-key precedence, numeric strings, zero/negative clamping and partial data.
- `openrouter_override_rounds_major_units_without_overflow` covers rounding and
  large finite amounts.
- `provider_metric_overrides_preserve_native_failures` retains failure behavior.
  The harness also verifies no credential writes and the unchanged cents-based
  shared formatter and unrelated USD progress line.

Focused corrected-path command:

```sh
cargo test --locked --test provider_metrics_override openrouter_override
```

**Remove only when:** the candidate's native output represents those USD amounts
correctly without these wrappers, including partial-data and large-number cases;
progress values and shared formatter semantics remain correct. Disable both AST
registrations for the native proof. Once proven, remove the whole correction-only
file and adapt the fixture's optional loading, retaining the behavioral tests.
A credits-endpoint authorization change alone does not resolve this units bug;
do not request a management key merely to make the fixture succeed.

### CU-002: DeepSeek balance currency

**Known-bad evidence:** pinned
[`findBalance` and `probe`](https://github.com/barramee27/crossusage/blob/ba1ad530b2148ba4be738332cb102858e9bdcc55/plugins/deepseek/plugin.js#L53-L119).
The provider's [balance schema](https://api-docs.deepseek.com/api/get-user-balance)
explicitly identifies `currency` as `CNY` or `USD` and `total_balance` as an amount
in that currency.

**Cause and effect:** `findBalance` prefers USD, otherwise returns a CNY number
without its currency. `probe` then unconditionally emits
`format: {kind: "dollars"}`, which means USD in our output contract. For a CNY
remaining balance `12.5` and initial balance `20`, the correct usage is `7.5 CNY`,
not `7.5 USD`. No conversion rate is implied by these numbers.

**Local scope:** [deepseek.js](../plugin-overrides/deepseek.js) wraps `findBalance`
with `patchFindBalance`, retaining native selection/numeric behavior while tracking
the selected currency in closure-private state. Its probe wrapper changes only
the CNY Balance progress format to `{kind: "currency", currency: "CNY"}`. State
is reset before and after every invocation, including failures. USD keeps
`{kind: "dollars"}`; unsupported/invalid balances keep native failure behavior.
`DEEPSEEK_INITIAL_BALANCE` must be in the selected balance's currency; it is not
a USD-equivalent setting. See [output units](json-output.md#progress-formats-and-units).

**Regression evidence:** [provider_metrics_override.rs](../tests/provider_metrics_override.rs):

- `deepseek_override_preserves_cny_without_conversion` checks exact `7.5 / 20`
  CNY progress and the unchanged authenticated request.
- `deepseek_override_preserves_usd_priority_and_per_probe_currency` checks USD
  preference, sequential CNY/USD/error/CNY cases and account isolation.
- `provider_metric_overrides_preserve_native_failures` retains invalid-data and
  authentication errors. Shared USD behavior and zero credential writes are
  asserted by the production-lifecycle harness.

Focused corrected-path command:

```sh
cargo test --locked --test provider_metrics_override deepseek_override
```

**Remove only when:** native selection preserves the selected currency through to
our serialized result and passes CNY-only, USD-preferred, malformed/unsupported
and sequential-invocation controls without this correction. An upstream fix that
merely rejects all CNY avoids mislabeling but does not preserve our approved CNY
output capability; accepting that reduction requires a separate decision. The
host may need an approved adapter for a changed native format, not currency
conversion. Once equivalence is proven, remove the AST registration/probe wrapper
and the whole correction-only file, preserving the regression assertions.

### CU-003: Codex reset-detail credentials

**Known-bad evidence:** pinned
[`probeWithAuthState` retry paths](https://github.com/barramee27/crossusage/blob/ba1ad530b2148ba4be738332cb102858e9bdcc55/plugins/codex/plugin.js#L1088-L1175)
and its
[reset-details request](https://github.com/barramee27/crossusage/blob/ba1ad530b2148ba4be738332cb102858e9bdcc55/plugins/codex/plugin.js#L1230-L1248).

**Cause and effect:** reactive refresh returns a new token to the usage retry,
but does not update the lexical `accessToken` later passed to
`fetchResetCredits`. The reload-then-refresh path has the same problem after its
second refresh. Usage can therefore succeed with a new token while the optional
`GET /rate-limit-reset-credits` uses an older one and loses expiry/details data.
Proactive refresh and an external reload alone update that local token; they are
controls, not separate demonstrated instances of the bug.

**Local scope:** [codex.js](../plugin-overrides/codex.js) has a small correction
inside its larger account-routing implementation:

| Native target | Callback | Correction responsibility |
| --- | --- | --- |
| `probeWithAuthState` | `patchProbeWithAuthState` | Create an invocation-local `probeFrame` and restore the previous frame in `finally`. |
| `fetchUsage` | `patchFetchUsage` | Record the token/account pair only after a 2xx usage response. |
| `fetchResetCredits` | `patchFetchResetCredits` | Use that successful pair for the optional detail GET; otherwise delegate unchanged. |

The frame must not survive a candidate/account or a failed invocation. The
correction neither reimplements HTTP nor changes native refresh, discovery,
source filtering or OpenCode credential persistence.

**Regression evidence:** [codex_override.rs](../tests/codex_override.rs):

- `codex_override_reset_details_follow_successful_usage_credentials` covers native
  and OpenCode sources with reactive, proactive, external-reload and
  reload-then-refresh scenarios; it checks exact usage/detail credentials,
  account header, GET method and refresh counts.
- `codex_override_detail_credentials_are_isolated_after_account_errors` and
  `codex_override_native_failed_candidate_does_not_leak_detail_credentials`
  protect against cross-account/candidate state leakage and retain optional
  detail-failure behavior.

Focused corrected-path commands:

```sh
cargo test --locked --test codex_override codex_override_reset_details_follow_successful_usage_credentials
cargo test --locked --test codex_override codex_override_detail_credentials_are_isolated_after_account_errors
cargo test --locked --test codex_override codex_override_native_failed_candidate_does_not_leak_detail_credentials
```

**Remove only when:** native execution uses the successful usage token/account
for reset details on every retry/reload path, without the three registrations
above, and the isolation controls still pass. Remove only those registrations,
three callback implementations, `probeFrame`, and their corresponding global
exports/top-level aliases if no longer needed. Reinspect dependencies before
excision in case later maintenance shares a callback.

**Retain:** `discoverAccounts`, native/OpenCode routing and isolation,
`reloadAuthState`, `saveAuth`, `refreshToken`, file-subscription and persistence
behavior, and their tests. These are first-party features, not the stale-token
workaround. An upstream fix to CU-003 is not permission to delete `codex.js`.

### CU-004: Antigravity CLI refresh destination

**Status:** active correction, independently reviewed and covered by integrated
CI. The maintained implementation is
[antigravity-cli.js](../plugin-overrides/antigravity-cli.js); its regressions are in
[antigravity_cli_override.rs](../tests/antigravity_cli_override.rs).
Synthetic caller fixtures do not establish live keyring interoperability or
acceptance of the entire provider update.

**Known-bad evidence:** pinned
[`readKeychainValue`](https://github.com/barramee27/crossusage/blob/ba1ad530b2148ba4be738332cb102858e9bdcc55/plugins/antigravity-cli/plugin.js#L53-L70),
[`writeKeychainAuth`](https://github.com/barramee27/crossusage/blob/ba1ad530b2148ba4be738332cb102858e9bdcc55/plugins/antigravity-cli/plugin.js#L185-L200)
and
[refresh persistence](https://github.com/barramee27/crossusage/blob/ba1ad530b2148ba4be738332cb102858e9bdcc55/plugins/antigravity-cli/plugin.js#L250-L284).

**Cause and effect:** native reads `(service="gemini", account="antigravity")`,
then falls back to service-only `gemini` on absence/read failure. If that fallback
selects another username or a username-less entry, a successful OAuth refresh
still calls `writeGenericPasswordForAccount("gemini", "antigravity", ...)`.
This can create another entry and leave the actual source stale, or overwrite an
existing explicit entry with another source's refreshed credentials. A locked
explicit destination instead fails to write. Successful explicit-account reads
are not affected by this selector mismatch.

The generic SDK is correct to treat a service-only read and an explicit-account
write as distinct operations. A global "latest entry for this service" routing
hack would break independent explicit writes and is not an acceptable fix.

**Local scope:** ordinary `wrapProbe`, with no AST registration. Each invocation
creates child context/host/keychain objects and private selector state. The
wrapped `readGenericPassword` remembers whether the latest successful relevant
read was service-only and clears the state on a new failed/absent read. Only the
native explicit refresh write to `gemini/antigravity` is adapted after that
service-only read: call the original `writeGenericPassword("gemini", payload)`
instead. Explicit-source refresh delegates to the original explicit writer.
No shared host method is mutated. Native parsing, HTTP, source precedence, retry
and write-failure warning behavior remain unchanged; a failed write does not
retry another destination.

**Regression gate:** eight tests exercise the actual pinned plugin through the
hermetic sandboxed production override lifecycle, with synthetic keychain/HTTP
hooks. The uncorrected control exposes wrong write selectors. These fixtures
exercise the native caller and maintained override, not a live keyring or the
Secret Service wire backend. The suite contains:

- `antigravity_cli_refresh_preserves_successful_read_selector`: explicit,
  service-only other-username and username-less sources, proactive/reactive refresh.
- `antigravity_cli_fallback_does_not_overwrite_existing_explicit_account`:
  fallback after explicit read failure preserves the other destination's bytes.
- `antigravity_cli_selected_write_failure_never_retargets`: one failed write,
  native warning and no alternate destination/creation.
- `antigravity_cli_non_refresh_and_native_errors_remain_unchanged`: native and
  corrected controls preserve meaningful output and failures.
- `antigravity_cli_failed_and_sequential_probes_do_not_reuse_selector` and
  `antigravity_cli_wrapper_does_not_retarget_independent_selectors_or_shared_api`:
  no invocation leakage or global/independent-write retargeting.
- `antigravity_cli_encoded_payload_refresh_keeps_native_parsing` and
  `antigravity_cli_reactive_refresh_uses_retry_read_not_initial_read`:
  native payload handling and the latest retry source remain authoritative.

The assertions include exact read/write selectors, OAuth fields, old/fresh request
tokens, complete write payloads, successful Session metrics, entry identity and
end-of-lifecycle write observations. SDK-only same-selector tests cannot replace
this native caller sequence. The focused command is:

```sh
cargo test --locked --test antigravity_cli_override
```

**Remove only when:** the candidate native code itself retains the successful
read destination and writes refreshed credentials to it, including fallback and
retry cases; the above tests pass with only this correction disabled. If the
resulting override remains correction-only, its whole file can then be removed.
Retain generic SDK identity and no-Prompt/Unlock tests; they protect a separate
contract. Record candidate-native evidence, independent removal review and
integrated checks before retirement.

### CU-005: Gemini Apps cookie-profile selection

**Status:** implemented in `plugin-overrides/gemini-apps.js`, independently
reviewed and integrated with `tests/gemini_apps_override.rs`. Five caller
regressions, fifteen reader/integration cases and integrated CI pass. This does
not establish live browser, keyring or Google interoperability.

**Known-bad evidence:** pinned
[`read_chromium_cookies`](https://github.com/barramee27/crossusage/blob/ba1ad530b2148ba4be738332cb102858e9bdcc55/crates/crossusage-core/src/chromium_cookies.rs#L59-L95),
[`pick_single_profile_cookies`](https://github.com/barramee27/crossusage/blob/ba1ad530b2148ba4be738332cb102858e9bdcc55/crates/crossusage-core/src/chromium_cookies.rs#L227-L243),
and the native
[`hasGeminiSession` and reader](https://github.com/barramee27/crossusage/blob/ba1ad530b2148ba4be738332cb102858e9bdcc55/plugins/gemini-apps/plugin.js#L56-L84).

**Cause and effect:** the plugin requests nine possible cookie names, but accepts
`__Secure-1PSID` together with either `SAPISID` or `__Secure-1PAPISID`. The generic
reader prefers all requested names, otherwise the map with the most names. An
earlier profile containing eight names but no `__Secure-1PSID` therefore defeats
a later two-cookie profile that the native session predicate accepts. The plugin
rejects the chosen profile and may incorrectly require `GEMINI_COOKIE` despite a
usable automatic source. This mismatch was reproduced with the unchanged pinned
Rust selection function and native JavaScript predicate on synthetic maps, not
with live browser profiles. A subsequent integrated fixture also reproduces the
failure with the real SQLite reader and native caller, then verifies corrected
metrics and exact selected cookies.

**Local scope:** preserve `host.chromiumCookies.read`. The general `readProfiles`
operation returns separate per-profile maps and keeps Google-specific session
rules out of the reader. Only the native `readChromeCookies` boundary is wrapped
by `patchReadChromeCookies`, delegating its unchanged native acceptance logic for
each candidate.
The first accepted profile wins; cookies are never stitched across profiles.
Native environment fallback, cookie/header construction, request and usage
parsing remain outside the correction. Ordinary reader implementation,
cryptography and private temporary storage are not temporary plugin hacks.

**Regression gate:** the five hermetic production-lifecycle tests cover:

- `gemini_apps_native_eight_cookie_profile_masks_valid_two_cookie_profile`:
  a functioning reader exists with the correction disabled; native authentication
  fails before HTTP, while the corrected caller yields meaningful typed metrics.
- `gemini_apps_first_valid_native_source_precedes_env_and_probes_are_isolated`:
  first-valid order, alternate SAPISID, no cross-profile stitching, native source
  priority, sequential probes and unchanged shared objects.
- `gemini_apps_native_env_errors_and_unsupported_controls_are_preserved` and
  `gemini_apps_native_http_failures_are_unchanged`: native fallback and reader,
  missing-source, malformed response and transport controls.
- `gemini_apps_single_valid_profile_preserves_native_success`: unchanged success
  when the reader's original choice already satisfies the native predicate.

Assertions include exact selected cookies, independently calculated SAPISIDHASH,
source, quotas, reset units and absence of leaked private state. These caller
fixtures inject synthetic reader maps. Separately,
`native_gemini_and_correction_consume_real_profile_maps` in the reader's unit
suite exercises actual SQLite profiles, the SDK binding, native plugin and
maintained override lifecycle with controlled roots/keys and synthetic HTTP.
It reproduces rejection of the eight-cookie profile, then requires three
requests using only the accepted two-cookie profile and exact quota values.
Neither fixture uses live browser/keyring/provider data or changes sandbox
permissions. A missing host API is not the bug's baseline.

```sh
cargo test --locked --test gemini_apps_override
cargo test --locked --lib plugin_engine::chromium_cookies
```

**Remove only when:** the candidate native caller/host combination selects a
usable single-profile session in the same cases with only CU-005 disabled,
without mixing profiles or changing fallback priority. Remove only its AST
registration and wrapper; if the file remains correction-only, remove the whole
file. Keep the caller regression as candidate-native coverage. The ordinary
reader and the additive `readProfiles` API are separate contracts; audit other
consumers before proposing their removal. Record the verified-good revision,
removal review and integrated checks; none has been established yet.

## What is not an upstream plugin workaround

- Codex/Copilot OpenCode multi-account discovery, origin filtering, source routing,
  credential persistence and file subscriptions are first-party capabilities.
- Adapting an AST hook after legitimate upstream refactoring is compatibility
  maintenance, not proof that the refactoring was an upstream defect.
- Closure-private registries and complete write-observation fixtures corrected
  our override/test defects; they are not CrossUsage plugin bugs.
- The inherited native pricing-loader correction and Secret Service transport
  findings are tracked in the update report and their component provenance;
  they are not JavaScript plugin override entries.
- An analyzed upstream defect with no active correction, such as an unused GUI
  credential-helper mismatch, must retain its separate impact disposition. Do not
  invent an active workaround or claim a fix for an unreachable feature.

Add a new stable entry before accepting any further upstream-plugin correction.
Each entry needs a verified snapshot and source evidence, triggering conditions,
user-visible effect, exact local seam, preserved behavior, behavioral regression
and explicit removal conditions. Never reuse a retired ID for another defect.
