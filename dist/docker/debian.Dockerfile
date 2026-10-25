# syntax=docker/dockerfile:1
FROM rust:1.99.1-trixie AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential libclang-dev pkg-config dpkg-dev \
    && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install --locked cargo-deb --version 3.6.3

WORKDIR /build
COPY Cargo.toml Cargo.lock README.md LICENSE ./
COPY src/ src/
COPY vendor/crossusage/ vendor/crossusage/
COPY plugin-overrides/ plugin-overrides/
COPY dist/scripts/check-deb-dependencies.sh /usr/local/bin/check-deb-dependencies

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release --locked \
    && rm -rf vendor/crossusage/plugins/mock \
    && mkdir /package \
    && cargo deb --locked --no-build --output /package/openusage-cli.deb \
    && sh /usr/local/bin/check-deb-dependencies /package/openusage-cli.deb > /package/depends.txt \
    && dpkg-deb --field /package/openusage-cli.deb > /package/control.txt \
    && cd /package && sha256sum openusage-cli.deb > SHA256SUMS

FROM debian:13 AS check
COPY --from=build /package/ /package/
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends /package/openusage-cli.deb \
    && apt-get check \
    && test "$(dpkg-query -W -f='${Status}' openusage-cli)" = 'install ok installed' \
    && test ! -e /usr/share/openusage-cli/plugins/mock \
    && /usr/bin/openusage-cli --version > /package/version.txt \
    && /usr/bin/openusage-cli query --use-daemon=false --type=plugins --app-data-dir=/tmp/package-smoke > /package/plugins.json \
    && /usr/bin/openusage-cli query --use-daemon=false --type=config --app-data-dir=/tmp/package-smoke > /package/config.json

# Export only artifacts whose installation and metadata checks completed.
FROM scratch AS package
COPY --from=check /package/ /
