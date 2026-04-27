# Development

## Compatibility strategy

`openusage-cli` prioritizes compatibility with upstream OpenUsage plugin contracts.

- Upstream references are vendored in `vendor/openusage/`
- Runtime compatibility fixes should be implemented in `src/` (not by editing vendored plugin files)

See [OPENUSAGE_DIFFERENCES.md](../OPENUSAGE_DIFFERENCES.md) for intentional behavior differences.

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
- Upstream plugins: `/usr/share/openusage-cli/openusage-plugins`
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
