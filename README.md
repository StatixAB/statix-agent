# statix-agent

Host-side Statix agent repository.

This repo contains:
- the Rust `statix` agent binary
- Ubuntu installer assets under `installers/ubuntu/24.04`
- the host-side systemd units and updater script

## Releases

The expected public release assets are:
- `statix-agent-linux-amd64`
- `statix-agent-linux-arm64`
- matching `.sha256` files
- installer assets for supported distributions

The Ubuntu installer defaults to:

```bash
https://github.com/statixab/statix-agent/releases/latest/download
```

## Local build

```bash
cargo build --release
```
