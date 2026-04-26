# Installation

`openusage-cli` currently supports Linux only.

## Packages

### Arch Linux (AUR)

Two AUR packages are available:

- `openusage-cli` (stable)
- `openusage-cli-git` (latest `main`)

Install with your AUR helper:

```bash
# stable
yay -S openusage-cli

# development
yay -S openusage-cli-git
```

### Debian-based distributions (`.deb`)

Download the latest `.deb` from GitHub Releases, then install:

```bash
sudo apt install ./openusage-cli_<version>_<arch>.deb
```

### RPM-based distributions (`.rpm`)

Download the latest `.rpm` from GitHub Releases, then install:

```bash
# Fedora / RHEL / Rocky / AlmaLinux
sudo dnf install ./openusage-cli-<version>-1.<arch>.rpm

# openSUSE
sudo zypper install ./openusage-cli-<version>-1.<arch>.rpm
```

## Build from source

```bash
cargo build --locked
```

Run directly from source checkout:

```bash
cargo run -- query
```

## Recommended first run

For continuous background refresh and fast query responses, use the user systemd mode:

```bash
openusage-cli install-systemd-unit
systemctl --user daemon-reload
systemctl --user enable --now openusage-cli.service
```

See `docs/daemon-modes.md` for standalone and systemd tradeoffs.
