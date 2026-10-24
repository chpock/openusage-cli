# Development

## Compatibility strategy

`openusage-cli` prioritizes compatibility with upstream CrossUsage plugin contracts.

- Upstream plugins and references are vendored verbatim in `vendor/crossusage/`
- The upstream repository, pinned commit, payload paths, and synchronization policy are recorded in [crossusage-upstream.md](crossusage-upstream.md)
- Follow [crossusage-update-workflow.md](crossusage-update-workflow.md) for plugin updates; execute it only with operator authorization
- Runtime compatibility fixes should be implemented in `src/` (not by editing vendored plugin files)

See [crossusage_differences.md](crossusage_differences.md) for intentional behavior differences.

## Verification

Full CI parity command:

```bash
make ci-compact
```

Fast local loop:

```bash
cargo fmt && cargo test
```

Focused tests:

```bash
cargo test --test http_smoke
cargo test --test plugin_compatibility
cargo test --test codex_override
```

## Build Linux packages

Install helper cargo subcommands once:

```bash
cargo install cargo-deb cargo-generate-rpm
```

Build packages:

```bash
make deb
make rpm
make packages
```

Package layout:

- Binary: `/usr/bin/openusage-cli`
- Upstream plugins: `/usr/share/openusage-cli/plugins`
- Override plugins: `/usr/share/openusage-cli/plugin-overrides`

## Release versioning

- Branches keep `Cargo.toml` at `0.0.0`
- Release version comes from Git tag `vX.Y.Z`
- CI injects tag version during packaging and exports `OPENUSAGE_BUILD_VERSION`

Create a release tag:

```bash
make release-tag VERSION=0.2.0
git push origin v0.2.0
```
