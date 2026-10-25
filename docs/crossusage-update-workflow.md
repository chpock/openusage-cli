# CrossUsage plugin update workflow

**Status: approved procedure; refine it using evidence from each update.**

This is a maintainer/agent procedure, not an automated update job. Its purpose is
to update the pinned CrossUsage plugins, adapt our runtime where necessary, and
record what was checked. Extend it with actual cases discovered during updates.
Creating or approving this document does not itself execute an update.

## Scope and invariants

- Source: <https://github.com/barramee27/crossusage.git>.
- Choose a published stable release, then freeze its tag and full commit SHA.
  Do not follow a moving branch during the update.
- `vendor/crossusage/` is a selected payload, not the entire upstream application.
  All its files must be unmodified copies from the same upstream commit. Keep
  the complete `plugins/` directory, including tests, helpers, and assets.
- Make compatibility changes in our Rust code and first-party overrides, never
  by patching vendored files. Do not import the upstream application wholesale.
- Preserve our documented intentional differences unless a change is explicitly
  approved; see [crossusage_differences.md](crossusage_differences.md).
- New public output formats, persistent storage, permissions, dependencies, or
  other material design changes need operator approval before implementation.
  Do not conceal missing compatibility with dummy APIs or discarded results.
- Do not commit, push, publish, deploy, or use real credentials without separate
  authorization. Do not change the ignored root `openusage/` reference checkout.

## Run inputs and evidence

Use one task directory outside maintained source files for the upstream checkout,
raw diffs, detailed review, and test logs. Follow the operator/harness destination
rules; otherwise agree on a working location. Use absolute paths in commands.

Record these values in the working report before changing files:

| Name | Meaning |
| --- | --- |
| `BASE` | This project's full commit SHA before the update |
| `OLD` | Currently vendored upstream SHA from [crossusage-upstream.md](crossusage-upstream.md) |
| `TAG` | Selected CrossUsage release tag, such as `vX.Y.Z` |
| `NEW` | Full commit SHA obtained by resolving `TAG^{commit}` |
| `UPSTREAM` | Absolute path to a separate CrossUsage checkout |
| `TASK` | Absolute path to this update's working directory |
| `BEFORE`, `AFTER` | Frozen copies of the old and proposed vendor payloads |

Minimum working evidence:

- `review.md`: release identity, payload inventory, API/plugin/override analysis,
  decisions, validation results, and final verdict.
- `vendor.diff`: complete raw before/after vendor diff and its checksum.
- Supplemental upstream contract diff when relevant code is outside our payload.
- Logs and fixtures referenced by the review, without secrets.

Keep the current source identity in `docs/crossusage-upstream.md`. At completion,
keep a concise release-specific report in `docs/crossusage-updates/<TAG>.md`:
changes, adaptations, verified behavior, remaining gaps, and accepted exceptions.
Temporary raw diffs and logs need not become repository files.

## 1. Establish the baseline and select the release

1. Obtain approval to execute this workflow. Confirm that the project working
   tree and index are clean; do not automatically stash or reset unrelated work.
2. Record `BASE`, `OLD`, and existing validation results. Verify that the current
   vendor payload matches its recorded upstream revision, including paths,
   bytes, and Git file modes. Investigate discrepancies before replacing it.
3. Select the latest published non-prerelease CrossUsage release at the time of
   selection, unless the operator specifies another release. Record its URL,
   version, publication date, and reason for selection.
4. Clone upstream into `UPSTREAM`, not into `vendor/`. Verify its origin. Fetch
   enough history to inspect both `OLD` and the selected release; avoid a shallow
   checkout that prevents the required comparison.
5. Resolve the tag to its commit and check out that exact commit detached:

   ```bash
   git -C "$UPSTREAM" rev-parse "refs/tags/$TAG^{commit}"
   git -C "$UPSTREAM" checkout --detach "$NEW"
   ```

   Record an annotated tag object's SHA separately from `NEW`. GitHub release
   `target_commitish` can be a branch name, not the released commit. If a reused
   tag resolves differently from a previously recorded value, stop and report it.
6. Check the release notes and version declarations. Release version, commit,
   and individual plugin manifest versions are different identifiers; unchanged
   plugin versions do not prove unchanged content. An old SHA-only snapshot
   must not be assigned an invented release version.

**Exit:** old and new immutable identities are known; the baseline is understood.

## 2. Prepare and synchronize the vendor payload

1. Preserve `BEFORE` from the clean project baseline.
2. Read the target tree and identify the current locations of plugins, contract
   docs, and reference host/manifest/runtime modules. Start from the inventory in
   `docs/crossusage-upstream.md`; do not assume those paths still exist.
3. Record the new payload inventory and any moved, added, or removed reference
   paths. Include relevant contract dependencies when the existing references
   are insufficient; do not bring in unrelated desktop application files.
4. Export `AFTER` from the pinned Git objects, rather than a working-tree copy
   possibly modified by upstream build/test scripts. Check entry scripts,
   manifests, and referenced assets; investigate missing/generated inputs.
5. Replace the selected vendor payload completely, removing stale files. Do not
   copy new files over old ones and leave upstream deletions behind. Verify
   the actual mirror against `AFTER` and `NEW`: exact file set, bytes, and modes.

**Exit:** vendor contains one reproducible snapshot, with no mixed revisions or
local changes. It is not yet declared compatible.

## 3. Record the proposed source revision

Update `docs/crossusage-upstream.md` with:

- selected release/version, release URL, full SHA, and upstream repository;
- previous SHA, current payload inventory, and relevant tree/file counts;
- compatibility status: **pending validation** until the remaining steps pass.

Keep initial migration evidence clearly historical, tied to its original SHA.
Do not leave old file counts or baseline-equivalence claims describing the new
snapshot. Update links to reference modules when their upstream paths move.

## 4. Freeze the change evidence

Save the complete vendor diff before any runtime adaptations. The diff is the
source of truth for changes in the selected payload; summaries do not replace it.
Use a comparison that includes added/untracked and removed files. For example,
compare the frozen directories, with `BEFORE` and `AFTER` naming their roots:

```bash
if git diff --no-index --binary --full-index --no-ext-diff --no-textconv \
    --no-renames "$BEFORE" "$AFTER" > "$TASK/vendor.diff"; then
    diff_status=0
else
    diff_status=$?
fi
test "$diff_status" -le 1 && \
    sha256sum "$TASK/vendor.diff" > "$TASK/vendor.diff.sha256"
```

Exit `1` means differences; a greater value means failure. Do not use a plain
worktree `git diff` as complete evidence when new files are untracked. For an
upstream comparison, use the two endpoints (`OLD NEW`), not a merge-base diff
(`OLD...NEW`). Keep raw changes even when a rename-aware view helps review.

Read relevant implementation dependencies and registration/build policy from
the complete upstream checkout. If they are outside our vendor payload, save
an additional scoped `OLD`-to-`NEW` diff and list its scope in the report. A
vendor-only diff cannot prove that code outside that selection is unchanged.

Tie analysis to these exact snapshots. If the target or payload changes later,
regenerate the affected evidence and revisit conclusions that depended on it.

## 5. Analyze plugin API compatibility

Compare old upstream, new upstream, and our implementation—not only two versions
of the upstream documentation. Inspect actual host code and plugin call sites.

Use one row per changed capability in `review.md`:

| Capability | Upstream change and evidence | Our behavior | Affected plugins | Required action and verification |
| --- | --- | --- | --- | --- |

Cover applicable changes to:

- plugin registration, manifest fields, entry loading, assets, and identifiers;
- `ctx`, host APIs, helpers, arguments, return values, errors, and side effects;
- synchronous/Promise execution, evaluation order, and timeout behavior;
- metric/output formats through parsing, serialization, CLI, HTTP, and cache;
- public response shapes, optionality and units against the first-party
  [JSON output reference](json-output.md), not upstream schemas alone;
- filesystem/env/auth access and new platform-specific prerequisites.

Classify each item: already compatible, adaptation required, preserved intentional
difference, or unresolved. Explain the effect of any proposed exception. Before
acceptance, reconcile every initial capability row against fresh final source and
its evidence: implemented/reviewed, equivalent unchanged behavior, explicitly
approved omission, or still open. Completing one subtask, such as execution
safety, does not close a row's separate dispatch, discovery or output obligations.
Analyze all API changes, but do not assume this update authorizes every upstream
product feature. A guarded call can still be the only useful source on a target
platform; distinguish absence of an exception from useful provider operation.
Check new credential/data access performed through existing generic APIs too.
Before porting an apparent API change, exercise the existing implementation on
representative inputs, including language/runtime built-ins. A source-level
change does not by itself establish a behavioral gap; reuse equivalent existing
behavior rather than adding a redundant adapter.
For internal caches or throttling, compare actual object/runtime lifetimes in
both hosts rather than assuming state survives refreshes. When cached values
come from authenticated sources, check how their keys identify the effective
account/source as well as provider and query; a TTL does not prevent serving a
previous login's values. Verify source changes and unchanged-source reuse with
request/recomputation counters.

For native data readers, verify source-selection semantics as well as record
parsing: traversal depth, permitted aliases, live/archive priority and the
meaning of a missing or invalid source root. Where data is selected from several
profiles, compare the reader's completeness rule with the caller's actual minimum
requirements. More fields do not necessarily mean a usable credential/session;
never construct one by joining unrelated profiles. For live database copies,
review journal/snapshot consistency separately from read-only SQL and private
file permissions; record any accepted limitation before implementation.
Apply resource budgets to distinct effective inputs, not duplicate paths. Verify mode-dependent pricing against
pinned data, including modifiers and already-priced variants; correct token
counts alone do not establish correct cost.

Trace the dependencies of required runtime code, including embedded data,
platform implementations and external helpers. Stable host method signatures do
not establish unchanged parser, pricing-data, storage-driver or discovery behavior.
An earlier reuse decision still needs demonstrated semantic equivalence or an
explicitly accepted difference, including runtime prerequisites. Revisit its
rationale when later dependency/scope decisions change the trade-off. A reference
module alone may not include everything required to implement it. Reuse suitable
existing code
and dependencies, obtain approval for necessary additions, and record the pinned
origin of imported code/data. Preserve the required copyright/license notices
in every installed package path as well as the source tree. Check that a
dependency's public API can express the
required interaction/error policy on every operation, including creation and
failure paths. Convenience methods may hide dialogs or other side effects.
Verify forbidden interactions with observable call counts; library approval does
not authorize relaxing constraints or silently dropping an operation.

For external helper jobs, verify candidate usability, not merely file existence
or a retained command name. A rejected candidate must not hide another valid
candidate; inspect installation-manager indirection and inherited PATH separately.
Availability probes must obey the same bounded execution and secret-handling
rules as the actual job. Verify its entire lifetime: input delivery, output
draining, inherited-pipe EOF, termination and cleanup—not just waiting for its
direct child. Test timeout and failure paths with descendants holding pipes;
cleanup must preserve unrelated processes and must not expose secrets.

When refreshed credentials must be written back, verify that storage operations
retain the selected source/account identity. A lookup selector or a default slot
is not necessarily the identity of the entry that was actually read.

Keep the output reference synchronized with approved contract changes, including
examples and explicit availability when an addition is specified before it is
implemented.

Defer implementation until the plugin/override impact review below establishes
what is needed and material design decisions are approved.

## 6. Analyze every affected plugin

Build an inventory keyed by manifest plugin ID. Include added, changed, removed,
and renamed plugins, plus otherwise unchanged plugins affected by shared API
changes. Compare effective upstream registration/retirement policy as well as
the directories present in `plugins/`.

For each affected plugin, record:

1. Changed files and behavior, with evidence from the diff and target sources.
2. New host API calls, platform/file/env dependencies, credential access, HTTP
   requests, response parsing, persistence, and output semantics as applicable.
3. Effect on this Linux utility: what should work, what requires adaptation,
   and what remains unverified. Check units, formats, and manifest/output
   agreement when changed; do not infer compatibility from successful loading.
4. A verification case and expected observable result for the changed behavior.
   Use upstream tests/provider contracts as evidence for expectations.
5. Effects of removed/renamed IDs on configuration, authentication-source
   selection, overrides, and package contents. Do not silently remap user config.

A passing aggregate provider-count check is not a complete inventory. Every
expected manifest must load, and every missing/excluded ID needs a disposition.
Track reviewed and uncovered IDs explicitly; a delivered partial analysis does
not close its uncovered rows. For runtime checks, record the actual execution
boundary per ID: backend unit, native caller with controlled readers, integrated
host path, or live replay. Passing evidence at one boundary does not establish
another. Complete the impact inventory before go/no-go.

## 7. Analyze our overrides

Inspect every first-party override, including when its plugin file is unchanged
but shared contracts changed. Start with Codex and Copilot, then check the actual
`plugin-overrides/` directory for additions. Read every active entry in the
[upstream workaround register](upstream-plugin-workarounds.md); distinguish
upstream repairs from first-party capabilities sharing the same override.

- Re-read each AST target in the target native plugin: existence, signature,
  return/state shape, call sites, and side effects. Matching function names
  alone do not prove that wrappers still work or intercept the intended path.
- Compare native execution without our override and execution with it. Preserve
  the intended native behavior except for the documented override additions.
- Verify our production lifecycle: transform, plugin evaluation, override
  evaluation, discovery, probes, and collected subscriptions. Do not substitute
  manual calls into override functions for lifecycle coverage.
- Preserve account routing, credential-source selection, refresh/write-back
  destination, error policy, and file subscriptions described in
  [plugin-overrides.md](plugin-overrides.md). Check the daemon's
  `ClearProvider -> discovery/probe -> ReplaceProvider` lifecycle when affected.
- Record removed targets, bypassed wrappers, overlapping new native account
  behavior, and potentially obsolete overrides. Removal or behavior changes need
  an explicit rationale and appropriate approval, not silent disabling.
- For each temporary repair, exercise its regression against candidate native
  behavior with only that repair disabled. Green corrected-path tests do not
  prove an upstream fix. Remove only verified-obsolete code, preserving unrelated
  features and semantic regression coverage; record the verified-good upstream
  revision and removal evidence in the register rather than deleting its history.
- Before accepting a new upstream-plugin repair, add a register entry with source
  evidence, triggering conditions, exact local seam, preserved behavior, tests and
  explicit removal conditions. Track planned versus implemented checks honestly.

## 8. Summarize impact and implement approved adaptations

Before changing runtime behavior, assemble the API, plugin, and override results
into a short go/no-go summary: required fixes, proposed design/contract changes,
known regressions, test plan, and unresolved questions.

- Resolve material scope/architecture/public-contract decisions with the
  operator before implementing them. Preserve decisions already settled unless
  new evidence changes their consequences.
- Implement compatibility fixes within the approved design in `src/` and
  first-party overrides. Add focused regressions for changed behavior and
  confirmed defects; establish a failing case before the corresponding fix
  where practical. Keep the vendor snapshot untouched.
- If implementation exposes another affected area, extend the impact analysis
  and validation scope. Do not broaden the update silently or weaken existing
  assertions just to obtain green tests.

## 9. Validate and write the final report

Keep normal sandbox and hermetic-data requirements. If a required integration
path cannot use the established isolation mechanism, obtain approval for a
narrowly bounded test alternative before running it. Disable unrelated probes
and sensitive side effects, record what the alternative does not exercise, and
do not extend that exception to ordinary provider probes.

1. Run focused tests for the changed host/runtime/output behavior. Use realistic
   success and failure fixtures, not only missing-auth early exits. Complete
   asynchronous work before asserting outcomes. Verify that test entrypoints
   initialize the same capability chain as production except for intentional
   isolation; fix harness defects rather than mistaking them for upstream changes
   or replacing the missing behavior with a successful stub. Bind build/run
   artifacts to the snapshot being checked; separate outputs when different
   snapshots are validated concurrently. Collect side-effect observations through
   the end of the operation, including work after its last network response; an
   early observation cannot prove that no later writes or requests occurred.
2. Run the upstream plugin tests with the target release's own test tooling and
   locked dependencies in the separate checkout. Record the exact command and
   result; do not assume its toolchain or run generators against our vendor.
3. Run our compatibility, HTTP, and both override suites:

   ```bash
   cargo test --locked --test plugin_compatibility --test http_smoke \
       --test codex_override --test copilot_override --test plugin_discovery
   make ci-compact
   ```

4. Exercise affected behavior through the real runtime/daemon and CLI/HTTP
   outputs using isolated fixtures or approved sanitized traffic replay. For
   cache/refresh checks, assert observable probe/request counts, not timings.
   Missing credentials or replay data remain a validation gap; an expected
   authentication error does not prove a successful provider interaction.
5. For real-filesystem tests, reuse `tests/support/hermetic.rs` and
   `runtime::run_probe_in_sandbox`. Do not read or modify the developer's real
   HOME/XDG/auth files. Observe the sandbox's intentionally disabled host APIs;
   it is not evidence that those APIs work. New tests may need controlled fakes.
   Keep production builders, validation and account routing intact when replacing
   sensitive reader boundaries; match exact inputs and reject unexpected calls
   even when the plugin catches their exceptions. Derive query fixtures from
   synthetic stores when the query contract matters. Label caller coverage
   separately from real reader/backend integration.
6. Verify actual package payloads and installed-binary discovery when plugin
   files, assets, IDs, or loading requirements changed. Check release mock
   exclusion, stale-file removal, override inclusion and required upstream
   copyright/license notices. Distinguish layout-only checks from full release
    or cross-architecture builds; inspect warnings and required metadata even if
   the packaging command returns zero. Record dependency-metadata overrides,
   missing tools and unrun required checks.
7. Obtain independent review of the actual final changes and supporting report.
   Verify findings against source; fix confirmed defects, explain rejected
   findings, and escalate unresolved material decisions. Re-run invalidated
   tests after fixes. Recheck verbatim vendor equality against `NEW`.

Finish `docs/crossusage-updates/<TAG>.md` with:

- old/new release identities and a summary of API/plugin/override changes;
- implemented adaptations and preserved intentional differences;
- verification results, evidence references, findings and their dispositions;
- remaining risks, unsupported/unverified behavior, and operator decisions;
- verdict: **verified**, **blocked**, or **accepted with explicit validation
  gaps**. An accepted risk must not be relabeled as a passed check.

Update the source record's compatibility status consistently. No final success
claim is valid while required checks fail or an unresolved blocker remains.
Commit/push/release remain separate authorized actions.

## Improving this workflow during a run

For an encountered problem, record **case -> condition/evidence -> action ->
verification** in the working report, then update the relevant step here when
it is reusable. Express additions as general conditions, actions, and verification
rules, not comparisons between specific files or fixes tied to one release or
provider. Keep concrete paths, diffs, and one-off decisions in the release report.
Amend inaccurate instructions as soon as evidence disproves them; pause for
approval if the correction changes the agreed scope, design, or acceptance
criteria. Do not turn an observation into a general rule without checking its
actual cause.

### Reusable cases

| Condition | Action and verification |
| --- | --- |
| Upstream reorganizes plugin or host-contract code. | Identify the current authoritative locations, update the payload inventory, remove obsolete reference copies, and verify the complete mirror against one commit. |
| The upstream Rust plugin API adds or changes capabilities. | Analyze their semantics and plugin callers; integrate the required behavior into our Rust API within the approved design, and verify the affected success and failure paths. Obtain approval for material contract changes. |
| A plugin is added or its behavior changes. | Determine how it works, inspect its required host functions and platform dependencies, and verify compatibility with our actual runtime rather than assuming all functions are implemented. |
| Effective upstream provider registration differs from the directories present. | Compare the active-provider policy with our inventory and document the disposition of each affected ID. |
| Plugins introduce new asset or output formats. | Check asset MIME, builders, parsing, actual serialized field names/metadata, and downstream consumers. Do not infer JSON names from source field names or type-level annotations; verify preservation of intended data rather than silent omission or misleading declarations. |
| Upstream tooling generates or rewrites plugin inputs. | Export the vendor payload from the pinned Git objects, keep generators separate from the mirror, and verify that testing/building has not changed the copied snapshot. Do not format or repair upstream whitespace in verbatim copies; distinguish inherited diagnostics from first-party failures. |
| A new guarded capability supplies the only useful input on a supported platform. | Trace all credential/data fallbacks and test a successful platform-specific path; do not call the provider compatible merely because an absent function is caught. |
| A native plugin uses existing generic APIs to access a new sensitive source. | Review effective credential access, transmission and source precedence; obtain any required permission decision even when no new host method is added. Verify with synthetic sources, not live credentials. |
| New native helpers perform their own network, filesystem or subprocess I/O. | Preserve existing proxy, sandbox, timeout, redaction and process-lifecycle policies; exercise those policies through the new helper, not only the older generic API. |
| Existing success fixtures become invalid under the target provider contract. | Replace them with authoritative successful response shapes and assert meaningful non-error metrics plus exact interactions. Retain the old invalid response as an explicit failure case; do not weaken production validation to make the test pass. |
| Upstream plugin behavior conflicts with its host contract or provider units. | Verify both implementations and authoritative provider evidence; distinguish an upstream defect from a local host gap. Decide a narrowly scoped correction or explicit limitation, preserving unrelated shared semantics and verbatim vendor content. |
| Enabling a previously absent capability makes another branch reachable. | Analyze and test the complete capability chain before enabling it, including output parsing, persistence and failures. Keep currently unreachable defects separate from existing runtime failures. |
| Cache or throttle behavior depends on state surviving between probes. | Trace actual lifetimes in both hosts and count requests or recomputations across lifecycle invocations. Do not assume persistence or redesign the runtime based only on a cache variable's name. |
| Required runtime code depends on other modules, embedded data or platform/helper implementations. | Trace the needed dependency closure before porting, agree necessary dependencies, preserve data provenance and required license notices in source and shipped artifacts, and verify actual availability. Do not satisfy imports with fake APIs or copy unrelated product features. |
| New optional metadata has a stricter public type than upstream input validation. | Normalize or reject input at the boundary according to the agreed output contract; document the policy and test mixed-valid/invalid, zero and absent values. Invalid optional metadata must not erase an otherwise valid metric. |
| An authenticated cache is keyed only by provider, query or TTL. | Bind cached data to effective account/source identity before serving it; verify source-switch invalidation and unchanged-source reuse with observable counters. |
| Review coverage is incomplete or contradicts the frozen inventory. | Reconcile each expected ID/capability with source evidence, correct unsupported classifications and complete uncovered entries before the compatibility decision. |
| A native reader selects files differently from upstream. | Verify traversal depth, canonical aliases, live/archive precedence and invalid-root errors with behavioral fixtures. Deduplicate effective inputs before resource limits so aliases neither inflate totals nor consume another input's budget. |
| Pricing data includes mode modifiers or explicit pre-priced variants. | Verify actual costs against the pinned data for base, modified and explicit-variant inputs; apply each modifier exactly once. Do not infer price correctness from token totals or model lookup alone. |
| A finding depends on library/platform edge behavior. | Check the pinned implementation and exercise the exact boundary on the supported runtime before accepting the claim. Distinguish a failing regression from protection of already-correct behavior; record retracted claims and untested platform limits explicitly. |
| A multi-profile reader and its caller use different completeness rules. | Exercise the actual minimum accepted credential/session shape against competing profiles. Keep profile identities separate; correct selection at the caller boundary or approve an additive reader contract rather than embedding provider-specific requirements into generic storage. |
| A native reader copies a live database and its journal independently. | Distinguish a private read-only copy from a consistent source snapshot. Verify journal/copy errors and cleanup; obtain a decision on native compatibility versus lock/snapshot guarantees, without claiming both. |
| An update may make a first-party upstream repair obsolete. | Recheck its registered invariant against candidate native behavior with only that repair disabled. Remove only proven-obsolete code, keep unrelated override features and regression coverage, and record the verified-good upstream revision and retirement evidence. |
| Passing tests replace a sensitive data-source boundary. | Record whether they prove a parser/backend, a native caller, or the integrated host path. Preserve real shared validation, match exact source/query inputs and reject unexpected interactions; do not infer backend or live interoperability from caller fixtures. |
| Packaging reports success but warns about omitted checks or metadata. | Inspect the actual artifact and required metadata, retain the warning, and report missing validation explicitly. Build and resolve dependencies in a matching distribution environment when the host cannot supply its tools. Check library requirements against generator policy and the distribution's dependency closure: a flat ELF library list is not the package dependency graph, and a base image can mask missing declarations. A zero exit code or successful installation alone does not establish complete metadata or every build/install target. |
| Imported runtime code or embedded data carries redistribution notices. | Preserve provenance and the required notices in every package layout; verify the installed notice bytes rather than only its presence in the repository. |
| A previously intermittent validation failure becomes reproducible. | Retain the original failure, compare controlled lifecycle phases and verify the cause against source. Seek approval for broader remediation when outside the agreed update; a passing rerun is neither a fix nor a waiver. |
