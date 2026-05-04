#!/usr/bin/env bash
set -Eeuo pipefail

readonly SERVICE_NAME="statix-agent"
readonly DEFAULT_DOWNLOAD_BASE_URL="https://github.com/statixab/statix-agent/releases/latest/download"

DOWNLOAD_BASE_URL="${STATIX_DOWNLOAD_BASE_URL:-$DEFAULT_DOWNLOAD_BASE_URL}"
BINARY_PATH="${STATIX_BINARY_PATH:-/usr/local/bin/statix-agent}"
VERSION_FILE="${STATIX_VERSION_FILE:-/opt/statix/version.json}"
SERVICE_PATH="${STATIX_SERVICE_PATH:-/etc/systemd/system/$SERVICE_NAME.service}"

log() {
  printf '[statix-updater] %s\n' "$*"
}

fail() {
  printf '[statix-updater] error: %s\n' "$*" >&2
  exit 1
}

detect_arch() {
  case "$(uname -m)" in
    x86_64 | amd64)
      printf 'amd64'
      ;;
    aarch64 | arm64)
      printf 'arm64'
      ;;
    *)
      fail "unsupported CPU architecture: $(uname -m)"
      ;;
  esac
}

download_file() {
  local url="$1"
  local destination="$2"
  curl -fsSL --retry 3 --retry-delay 2 "$url" -o "$destination"
}

verify_sha256() {
  local binary_path="$1"
  local sha_url="$2"
  local sha_file
  sha_file="$(mktemp)"

  if ! download_file "$sha_url" "$sha_file"; then
    rm -f "$sha_file"
    fail "checksum not available at $sha_url"
  fi

  local expected
  expected="$(awk '{print $1}' "$sha_file")"
  rm -f "$sha_file"

  if [[ ! "$expected" =~ ^[0-9a-fA-F]{64}$ ]]; then
    fail "invalid checksum from $sha_url"
  fi

  local actual
  actual="$(sha256sum "$binary_path" | awk '{print $1}')"
  if [[ "${actual,,}" != "${expected,,}" ]]; then
    fail "checksum mismatch"
  fi
}

main() {
  [[ "${EUID}" -eq 0 ]] || fail "updater must run as root"
  command -v curl >/dev/null 2>&1 || fail "curl is required"
  command -v systemctl >/dev/null 2>&1 || fail "systemctl is required"
  command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required"

  DOWNLOAD_BASE_URL="${DOWNLOAD_BASE_URL%/}"
  local arch binary_url temporary backup version_url version_tmp
  arch="$(detect_arch)"
  binary_url="${STATIX_AGENT_BINARY_URL:-$DOWNLOAD_BASE_URL/statix-agent-linux-$arch}"
  temporary="$(mktemp)"
  backup="$(mktemp)"

  log "downloading $binary_url"
  download_file "$binary_url" "$temporary"
  verify_sha256 "$temporary" "$binary_url.sha256"
  chmod 0755 "$temporary"

  if [[ -x "$BINARY_PATH" ]]; then
    cp "$BINARY_PATH" "$backup"
  else
    rm -f "$backup"
  fi

  systemctl stop "$SERVICE_NAME"
  install -m 0755 "$temporary" "$BINARY_PATH"

  version_url="${STATIX_VERSION_URL:-$DOWNLOAD_BASE_URL/version.json}"
  version_tmp="$(mktemp)"
  if download_file "$version_url" "$version_tmp"; then
    install -d -m 0755 "$(dirname "$VERSION_FILE")"
    install -m 0644 "$version_tmp" "$VERSION_FILE"
  fi
  rm -f "$version_tmp" "$temporary"

  systemctl daemon-reload
  systemctl start "$SERVICE_NAME"
  if ! systemctl is-active --quiet "$SERVICE_NAME"; then
    log "new agent did not start; restoring previous binary"
    if [[ -s "$backup" ]]; then
      install -m 0755 "$backup" "$BINARY_PATH"
      systemctl start "$SERVICE_NAME" || true
    fi
    rm -f "$backup"
    fail "updated agent failed to start"
  fi

  if [[ -f "$SERVICE_PATH" ]]; then
    systemctl enable "$SERVICE_NAME" >/dev/null
  fi

  rm -f "$backup"
  log "update complete"
}

main "$@"
