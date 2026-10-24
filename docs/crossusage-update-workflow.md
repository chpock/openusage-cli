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
- filesystem/env/auth access and new platform-specific prerequisites.

Classify each item: already compatible, adaptation required, preserved intentional
difference, or unresolved. Explain the effect of any proposed exception.
Analyze all API changes, but do not assume this update authorizes every upstream
product feature. Defer implementation until the plugin/override impact review
below establishes what is needed and material design decisions are approved.

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

## 7. Analyze our overrides

Inspect every first-party override, including when its plugin file is unchanged
but shared contracts changed. Start with Codex and Copilot, then check the actual
`plugin-overrides/` directory for additions.

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

1. Run focused tests for the changed host/runtime/output behavior. Use realistic
   success and failure fixtures, not only missing-auth early exits. Complete
   asynchronous work before asserting outcomes.
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
6. Verify package payloads and installed-binary discovery when plugin files,
   assets, IDs, or loading requirements changed. Check release mock exclusion
   and stale-file removal. Distinguish layout-only checks from full release or
   cross-architecture builds; record missing tools and unrun required checks.
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
| Plugins introduce new asset or output formats. | Check loading, parsing, serialized metadata, and downstream consumers; verify preservation of the intended data rather than silent omission or misleading type declarations. |
| Upstream tooling generates or rewrites plugin inputs. | Export the vendor payload from the pinned Git objects, keep generators separate from the mirror, and verify that testing/building has not changed the copied snapshot. |
