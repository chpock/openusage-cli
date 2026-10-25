# CrossUsage upstream snapshot

## Source revision

- Repository: <https://github.com/barramee27/crossusage.git>
- Release: [`v1.6.1`](https://github.com/barramee27/crossusage/releases/tag/v1.6.1), published 2026-10-20
- Commit: [`ba1ad530b2148ba4be738332cb102858e9bdcc55`](https://github.com/barramee27/crossusage/commit/ba1ad530b2148ba4be738332cb102858e9bdcc55)
- Commit date: 2026-10-20
- Commit subject: `Merge pull request #40 from barramee27/perf/optimize-build-and-size`
- Annotated tag object: `e7e333392a7ed7f419f0564b60478152b58a0a92`
- Previous snapshot: `bee03a908fe70f9e6f84b2f20b41115542c7c188` (SHA-only baseline)
- Upstream `plugins/` Git tree: `4a5dcff619a474a2052b9c699fce42c6db7a821b`
- Compatibility status: **approved scope implemented and synthetically validated; acceptance pending**; see [the update report](crossusage-updates/v1.6.1.md) for remaining live/build/package limits.

CrossUsage is the source for future plugin updates. The snapshot is pinned to a
commit, not a moving branch or a release tag that could later be moved.

## Vendored payload

`vendor/crossusage/` contains these paths from that single upstream commit:

- `LICENSE`
- `README.md`
- `plugins/` (the complete directory, including tests and shared helpers)
- `docs/plugins/api.md`
- `docs/plugins/schema.md`
- `crates/crossusage-core/src/plugin_engine/host_api.rs`
- `crates/crossusage-core/src/plugin_engine/manifest.rs`
- `crates/crossusage-core/src/plugin_engine/mod.rs`
- `crates/crossusage-core/src/plugin_engine/runtime.rs`

This is a selected payload, not a checkout of the entire desktop application.
All 127 files are unmodified upstream copies: 119 plugin files covering 29
manifest IDs, plus eight documentation/license/reference files. Upstream
documentation, license, and historical project names are preserved verbatim.
The vendored Rust files are compatibility references; the application builds
the root Rust crate.

Provenance is recorded here, outside the verbatim vendor mirror.

## Initial source migration (historical)

The original migration from OpenUsage to CrossUsage selected commit
`bee03a908fe70f9e6f84b2f20b41115542c7c188`. The evidence below concerns that
initial snapshot, not the current release.

The pre-migration plugin snapshot at this repository's commit
`01a77bfdd51c91c62ae22b6bcfad79db4be07909` has the same Git tree as that
CrossUsage commit's `plugins/` directory: all 73 files and 18 providers match,
including file names, content, and Git file modes. The initial source migration
introduced no plugin changes. All eight non-plugin reference files also matched
the pre-migration snapshot exactly.

Provider additions bounded the initial search: Kiro and Synthetic were already
present, while Command Code was absent. In the default branch's first-parent
history, these bounds are commit `fc8f0ae20eec999beefbaa5050866c6c6cda2661`
(inclusive) and commit `aa36185cc96de01b65bcd065d034357321a6df39` (exclusive).
The ancestry window `aa36185^ --not fc8f0ae^` contains 180 commits, including
merged side branches. A check of all 752 ancestors of the then-current default
branch head, `ba1ad530b2148ba4be738332cb102858e9bdcc55`, found ten exact
matches, all within this window.

None of the exact matches is on the default branch's first-parent chain. Four
are ancestry-maximal (none is an ancestor of another exact match):

- `7cf7a6f68da833893099851a969a458db11100eb`
- `7afc4fe71aa8e7217167614c10c4823bac2328a5`
- `bee03a908fe70f9e6f84b2f20b41115542c7c188`
- `a12292d0db436814bad082ddfd12cb28cafc895c`

The initial migration selected the revision with the latest committer timestamp
among these four equivalent plugin states. Timestamp is only a tie-break between independent
exact matches, not a substitute for ancestry or content comparison.

## Synchronization policy

Follow [crossusage-update-workflow.md](crossusage-update-workflow.md) for the
end-to-end update procedure and required evidence.

1. Choose a CrossUsage commit and inspect changes to plugins, host contracts,
   and functions patched by `plugin-overrides/`.
2. Replace the entire payload listed above from that same commit, removing
   stale files. Preserve upstream bytes and file modes; implement compatibility
   changes in `src/` or first-party overrides, not in the vendor mirror.
3. Update the revision, payload inventory, and any compatibility statements in
   this document. Recheck [host differences](crossusage_differences.md).
4. Run `cargo test --test plugin_compatibility`, `cargo test --test http_smoke`,
   both Codex and Copilot override suites, and `make ci-compact`.

Packages install plugins into `/usr/share/openusage-cli/plugins` (or
`<prefix>/share/openusage-cli/plugins`), independently of the upstream vendor's
name. The executable, configuration paths, and `__openusage_*` JavaScript ABI
identifiers retain their existing names.
