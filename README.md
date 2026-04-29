# statix-agent

Host-side Statix agent repository.

This repo contains:
- the Rust `statix` agent binary
- Ubuntu installer assets under `installers/ubuntu/24.04`
- Arch Linux installer assets under `installers/archlinux`
- the host-side systemd units and updater script

## Releases

The expected public release assets are:
- `statix-agent-linux-amd64`
- `statix-agent-linux-arm64`
- matching `.sha256` files
- distro-specific installer assets for supported distributions, for example:
- `statix-agent-install-ubuntu-24.04.sh`
- `statix-agent-update-ubuntu-24.04.sh`
- `statix-agent-install-archlinux.sh`
- `statix-agent-update-archlinux.sh`

Installer assets should be published under:

```bash
https://github.com/statixab/statix-agent/releases/latest/download
```

Public docs or bootstrap scripts in the `statix` repo should select the correct
installer asset for the target distribution instead of assuming a universal
Linux `install.sh` or `update.sh`.

## Local build

```bash
cargo build --release
```

For release packaging, use:

```bash
bash scripts/build-release.sh all
```

This stages structured output under `dist/release/` and writes the flat GitHub
release asset set under `dist/upload/`.
