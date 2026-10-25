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

### Debian 13 container build and installation check

```bash
make deb-docker
```

This requires a running Docker daemon with BuildKit and network access to the
image registry, Debian repositories and crates.io. Rust, cargo-deb and Debian
build tools are installed only inside the build image, not on the host. The
Docker CLI executable can be overridden with `DOCKER=/path/to/docker`.

The build uses the official `rust:1.99.1-trixie` image and cargo-deb 3.6.3. It
builds the binary and package in Debian 13 with `Cargo.lock` enforced, including
`dpkg-dev` for automatic shared-library dependencies. Empty `Depends` or failed
metadata extraction is a build error, not a successful layout-only result.
Cargo-deb 3.6.3 deliberately omits explicit `libgcc-*` dependencies; Debian 13's
`libc6` depends on `libgcc-s1`, so apt installs that runtime transitively. `Depends`
is therefore not a flat listing of every ELF `NEEDED` library. See the pinned
[dependency resolver](https://github.com/kornelski/cargo-deb/blob/v3.6.3/src/dependencies.rs).

A separate `debian:13` stage installs the package and its declared dependencies
with apt, checks package-manager consistency, and runs the installed binary's
version, plugin-metadata and configuration commands. These checks do not probe
providers or access host accounts. Only artifacts from the successful check stage
are exported:

- `target/debian-docker/openusage-cli.deb`
- `target/debian-docker/depends.txt` and `control.txt`
- `target/debian-docker/SHA256SUMS`
- `target/debian-docker/version.txt`, `plugins.json` and `config.json`

The container receives a whitelisted build context, not host Cargo caches,
credentials, application data or the existing `target/`. Source files are copied
into the image, not mounted writable. Mock exclusion happens only in that copy;
the maintained vendor mirror and host Rust builds are unchanged. Docker retains
its own image/build cache. A failed invocation returns nonzero; artifacts from a
previous successful invocation may still exist, so do not treat their presence as
proof that a later build passed.

This builds for Docker's selected platform; other architectures require a working
native builder or emulation and separate validation. A Debian 13 build/install
check is not a test of older Debian/Ubuntu releases or every optional provider
helper. Image tags and apt repositories can change, so this is a repeatable build
procedure, not a claim of byte-identical builds. The existing Ubuntu-based release
workflow and the direct `make deb`/`make rpm` commands remain unchanged.

## Release versioning

- Branches keep `Cargo.toml` at `0.0.0`
- Release version comes from Git tag `vX.Y.Z`
- CI injects tag version during packaging and exports `OPENUSAGE_BUILD_VERSION`

Create a release tag:

```bash
make release-tag VERSION=0.2.0
git push origin v0.2.0
```
