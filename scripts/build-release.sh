#!/usr/bin/env bash
set -Eeuo pipefail

MODE="${1:-all}"
OUTPUT_ROOT="${OUTPUT_ROOT:-dist}"
RELEASE_ROOT="${OUTPUT_ROOT}/release"
UPLOAD_ROOT="${OUTPUT_ROOT}/upload"
LINUX_ROOT="${RELEASE_ROOT}/linux"
INSTALLER_ROOT="${RELEASE_ROOT}/installers/ubuntu/24.04"
METADATA_ROOT="${RELEASE_ROOT}/metadata"
BINARY_NAME="statix"

log() {
  printf '[build-release] %s\n' "$*"
}

fail() {
  printf '[build-release] error: %s\n' "$*" >&2
  exit 1
}

detect_asset_arch() {
  local candidate="${ASSET_ARCH:-}"

  if [[ -n "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return
  fi

  candidate="${RUST_TARGET:-}"
  case "$candidate" in
    x86_64-unknown-linux-gnu)
      printf 'amd64\n'
      ;;
    aarch64-unknown-linux-gnu)
      printf 'arm64\n'
      ;;
    "")
      case "$(uname -m)" in
        x86_64 | amd64)
          printf 'amd64\n'
          ;;
        aarch64 | arm64)
          printf 'arm64\n'
          ;;
        *)
          fail "unsupported host architecture: $(uname -m)"
          ;;
      esac
      ;;
    *)
      fail "unsupported Rust target for asset arch detection: ${candidate}"
      ;;
  esac
}

detect_rust_target() {
  if [[ -n "${RUST_TARGET:-}" ]]; then
    printf '%s\n' "$RUST_TARGET"
    return
  fi

  cargo -vV | awk '/^host:/ { print $2; exit }'
}

binary_path_for_target() {
  local rust_target="$1"

  if [[ -n "${RUST_TARGET:-}" ]]; then
    printf 'target/%s/release/%s\n' "$rust_target" "$BINARY_NAME"
    return
  fi

  printf 'target/release/%s\n' "$BINARY_NAME"
}

prepare_dirs() {
  mkdir -p "$LINUX_ROOT" "$INSTALLER_ROOT" "$METADATA_ROOT" "$UPLOAD_ROOT"
}

build_binary_assets() {
  local asset_arch rust_target binary_dir binary_path flat_binary flat_sha
  asset_arch="$(detect_asset_arch)"
  rust_target="$(detect_rust_target)"
  binary_dir="${LINUX_ROOT}/${asset_arch}"
  binary_path="$(binary_path_for_target "$rust_target")"
  flat_binary="${UPLOAD_ROOT}/statix-agent-linux-${asset_arch}"
  flat_sha="${flat_binary}.sha256"

  mkdir -p "$binary_dir"

  if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
    log "building ${rust_target}"
    if [[ -n "${RUST_TARGET:-}" ]]; then
      cargo build --locked --release --target "$rust_target"
    else
      cargo build --locked --release
    fi
  fi

  [[ -x "$binary_path" ]] || fail "missing built binary: ${binary_path}"

  install -m 0755 "$binary_path" "${binary_dir}/statix-agent-linux-${asset_arch}"
  sha256sum "${binary_dir}/statix-agent-linux-${asset_arch}" > "${binary_dir}/statix-agent-linux-${asset_arch}.sha256"

  install -m 0755 "${binary_dir}/statix-agent-linux-${asset_arch}" "$flat_binary"
  install -m 0644 "${binary_dir}/statix-agent-linux-${asset_arch}.sha256" "$flat_sha"
}

build_shared_assets() {
  local built_at git_tag git_sha

  install -m 0755 installers/ubuntu/24.04/install.sh "${INSTALLER_ROOT}/statix-agent-install-ubuntu-24.04.sh"
  install -m 0755 installers/ubuntu/24.04/update.sh "${INSTALLER_ROOT}/statix-agent-update-ubuntu-24.04.sh"
  install -m 0644 installers/ubuntu/24.04/statix-agent.service "${INSTALLER_ROOT}/statix-agent.service"
  install -m 0644 installers/ubuntu/24.04/statix-agent-update.service "${INSTALLER_ROOT}/statix-agent-update.service"

  built_at="${BUILT_AT:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
  git_tag="${GIT_TAG:-$(git describe --tags --exact-match 2>/dev/null || printf 'dev')}"
  git_sha="${GIT_SHA:-$(git rev-parse HEAD)}"

  cat > "${METADATA_ROOT}/version.json" <<EOF
{
  "version": "${git_tag}",
  "commit": "${git_sha}",
  "builtAt": "${built_at}"
}
EOF

  install -m 0755 "${INSTALLER_ROOT}/statix-agent-install-ubuntu-24.04.sh" "${UPLOAD_ROOT}/statix-agent-install-ubuntu-24.04.sh"
  install -m 0755 "${INSTALLER_ROOT}/statix-agent-update-ubuntu-24.04.sh" "${UPLOAD_ROOT}/statix-agent-update-ubuntu-24.04.sh"
  install -m 0644 "${INSTALLER_ROOT}/statix-agent.service" "${UPLOAD_ROOT}/statix-agent.service"
  install -m 0644 "${INSTALLER_ROOT}/statix-agent-update.service" "${UPLOAD_ROOT}/statix-agent-update.service"
  install -m 0644 "${METADATA_ROOT}/version.json" "${UPLOAD_ROOT}/version.json"
}

prepare_dirs

case "$MODE" in
  all)
    build_binary_assets
    build_shared_assets
    ;;
  binary)
    build_binary_assets
    ;;
  shared)
    build_shared_assets
    ;;
  *)
    fail "unknown mode: ${MODE}"
    ;;
esac
