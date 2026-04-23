#!/usr/bin/env bash
set -Eeuo pipefail

readonly SERVICE_NAME="statix-agent"
readonly SERVICE_USER="statix-agent"
readonly SERVICE_GROUP="statix-agent"
readonly DEFAULT_DOWNLOAD_BASE_URL="https://github.com/statixab/statix-agent/releases/latest/download"
readonly UPDATE_SCRIPT_ASSET_NAME="statix-agent-update-ubuntu-24.04.sh"

DOWNLOAD_BASE_URL="${STATIX_DOWNLOAD_BASE_URL:-$DEFAULT_DOWNLOAD_BASE_URL}"
INSTALL_DIR="${STATIX_INSTALL_DIR:-/usr/local/bin}"
BINARY_PATH="${STATIX_BINARY_PATH:-$INSTALL_DIR/statix}"
CONFIG_DIR="${STATIX_CONFIG_DIR:-/etc/statix}"
CONFIG_PATH="${STATIX_AGENT_CONFIG:-$CONFIG_DIR/agent.json}"
ENV_FILE="${STATIX_AGENT_ENV_FILE:-$CONFIG_DIR/agent.env}"
VERSION_FILE="${STATIX_VERSION_FILE:-/opt/statix/version.json}"
SERVICE_PATH="${STATIX_SERVICE_PATH:-/etc/systemd/system/$SERVICE_NAME.service}"
UPDATE_SCRIPT_PATH="${STATIX_UPDATE_SCRIPT_PATH:-/usr/local/lib/statix/update.sh}"
UPDATE_SERVICE_PATH="${STATIX_UPDATE_SERVICE_PATH:-/etc/systemd/system/$SERVICE_NAME-update.service}"
SUDOERS_PATH="${STATIX_AGENT_SUDOERS_PATH:-/etc/sudoers.d/$SERVICE_NAME}"
API_BASE_URL="${STATIX_API_BASE_URL:-}"
NODE_NAME="${STATIX_NODE_NAME:-}"
SKIP_LOGIN="${STATIX_SKIP_LOGIN:-0}"
SKIP_START="${STATIX_SKIP_START:-0}"

log() {
  printf '[statix-installer] %s\n' "$*"
}

fail() {
  printf '[statix-installer] error: %s\n' "$*" >&2
  exit 1
}

require_root() {
  if [[ "${EUID}" -ne 0 ]]; then
    fail "run this installer as root, for example: curl -fsSL <url> | sudo bash"
  fi
}

require_systemd() {
  command -v systemctl >/dev/null 2>&1 || fail "systemctl is required"
}

require_sudo() {
  command -v sudo >/dev/null 2>&1 || fail "sudo is required"
}

normalize_download_base_url() {
  DOWNLOAD_BASE_URL="${DOWNLOAD_BASE_URL%/}"
}

detect_arch() {
  local machine
  machine="$(uname -m)"

  case "$machine" in
    x86_64 | amd64)
      printf 'amd64'
      ;;
    aarch64 | arm64)
      printf 'arm64'
      ;;
    *)
      fail "unsupported CPU architecture: $machine"
      ;;
  esac
}

check_platform() {
  if [[ ! -r /etc/os-release ]]; then
    fail "cannot detect OS release"
  fi

  # shellcheck disable=SC1091
  . /etc/os-release

  if [[ "${ID:-}" != "ubuntu" ]]; then
    log "warning: this installer targets Ubuntu 24.04; detected ${PRETTY_NAME:-unknown Linux}"
    return
  fi

  if [[ "${VERSION_ID:-}" != "24.04" ]]; then
    log "warning: this installer is tested on Ubuntu 24.04; detected ${PRETTY_NAME:-Ubuntu}"
  fi
}

install_dependencies() {
  command -v apt-get >/dev/null 2>&1 || fail "apt-get is required"

  export DEBIAN_FRONTEND=noninteractive
  log "installing dependencies"
  apt-get update
  apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    iproute2 \
    pciutils \
    wireguard-tools
}

download_file() {
  local url="$1"
  local destination="$2"
  curl -fsSL --retry 3 --retry-delay 2 "$url" -o "$destination"
}

install_agent_binary() {
  local arch binary_url temporary
  arch="$(detect_arch)"
  binary_url="${STATIX_AGENT_BINARY_URL:-$DOWNLOAD_BASE_URL/statix-agent-linux-$arch}"
  temporary="$(mktemp)"

  log "downloading agent binary from $binary_url"
  download_file "$binary_url" "$temporary" || fail "failed to download agent binary"

  install -d -m 0755 "$INSTALL_DIR"
  install -m 0755 "$temporary" "$BINARY_PATH"
  rm -f "$temporary"

  log "installed $BINARY_PATH"
}

ensure_service_account() {
  if ! getent group "$SERVICE_GROUP" >/dev/null 2>&1; then
    log "creating group $SERVICE_GROUP"
    groupadd --system "$SERVICE_GROUP"
  fi

  if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
    log "creating user $SERVICE_USER"
    useradd \
      --system \
      --gid "$SERVICE_GROUP" \
      --home-dir /nonexistent \
      --no-create-home \
      --shell /usr/sbin/nologin \
      "$SERVICE_USER"
  fi
}

install_version_file() {
  local version_url temporary version_dir
  version_url="${STATIX_VERSION_URL:-$DOWNLOAD_BASE_URL/version.json}"
  version_dir="$(dirname "$VERSION_FILE")"
  temporary="$(mktemp)"

  if download_file "$version_url" "$temporary"; then
    install -d -m 0755 "$version_dir"
    install -m 0644 "$temporary" "$VERSION_FILE"
    log "installed $VERSION_FILE"
  else
    log "version file not available at $version_url; continuing"
  fi

  rm -f "$temporary"
}

install_service_file() {
  local service_url update_service_url temporary update_temporary
  service_url="${STATIX_SERVICE_URL:-$DOWNLOAD_BASE_URL/statix-agent.service}"
  update_service_url="${STATIX_UPDATE_SERVICE_URL:-$DOWNLOAD_BASE_URL/statix-agent-update.service}"
  temporary="$(mktemp)"
  update_temporary="$(mktemp)"

  log "downloading systemd unit from $service_url"
  download_file "$service_url" "$temporary" || fail "failed to download systemd unit"
  log "downloading update systemd unit from $update_service_url"
  download_file "$update_service_url" "$update_temporary" || fail "failed to download update systemd unit"

  install -m 0644 "$temporary" "$SERVICE_PATH"
  install -m 0644 "$update_temporary" "$UPDATE_SERVICE_PATH"
  rm -f "$temporary"
  rm -f "$update_temporary"

  systemctl daemon-reload
  systemctl enable "$SERVICE_NAME"
  log "installed and enabled $SERVICE_NAME"
}

install_update_script() {
  local update_url temporary update_dir
  update_url="${STATIX_UPDATE_SCRIPT_URL:-$DOWNLOAD_BASE_URL/$UPDATE_SCRIPT_ASSET_NAME}"
  update_dir="$(dirname "$UPDATE_SCRIPT_PATH")"
  temporary="$(mktemp)"

  log "downloading updater from $update_url"
  download_file "$update_url" "$temporary" || fail "failed to download updater"

  install -d -m 0755 "$update_dir"
  install -m 0755 "$temporary" "$UPDATE_SCRIPT_PATH"
  rm -f "$temporary"
}

write_environment_file() {
  install -d -m 0750 "$CONFIG_DIR"
  touch "$ENV_FILE"
  chmod 0640 "$ENV_FILE"

  {
    printf 'STATIX_AGENT_CONFIG=%s\n' "$CONFIG_PATH"
    printf 'STATIX_VERSION_FILE=%s\n' "$VERSION_FILE"
    printf 'STATIX_DOWNLOAD_BASE_URL=%s\n' "$DOWNLOAD_BASE_URL"
  } >"$ENV_FILE"
}

set_agent_file_ownership() {
  chown -R "$SERVICE_USER:$SERVICE_GROUP" "$CONFIG_DIR"
}

install_update_sudoers() {
  local temporary
  temporary="$(mktemp)"

  cat >"$temporary" <<EOF
$SERVICE_USER ALL=(root) NOPASSWD: /usr/bin/systemctl start $SERVICE_NAME-update.service
EOF

  install -d -m 0755 "$(dirname "$SUDOERS_PATH")"
  install -m 0440 "$temporary" "$SUDOERS_PATH"
  rm -f "$temporary"
}

run_login() {
  if [[ "$SKIP_LOGIN" == "1" || "$SKIP_LOGIN" == "true" ]]; then
    log "skipping enrollment because STATIX_SKIP_LOGIN=$SKIP_LOGIN"
    return
  fi

  local login_args=()
  if [[ -n "$API_BASE_URL" ]]; then
    login_args+=(--api-base-url "$API_BASE_URL")
  fi
  if [[ -n "$NODE_NAME" ]]; then
    login_args+=(--name "$NODE_NAME")
  fi

  log "starting node enrollment"
  STATIX_AGENT_CONFIG="$CONFIG_PATH" STATIX_VERSION_FILE="$VERSION_FILE" "$BINARY_PATH" login "${login_args[@]}"
}

start_service() {
  if [[ "$SKIP_START" == "1" || "$SKIP_START" == "true" ]]; then
    log "skipping service start because STATIX_SKIP_START=$SKIP_START"
    return
  fi

  log "starting $SERVICE_NAME"
  systemctl restart "$SERVICE_NAME"
  systemctl --no-pager --full status "$SERVICE_NAME" || true
}

main() {
  require_root
  require_systemd
  require_sudo
  normalize_download_base_url
  check_platform
  install_dependencies
  ensure_service_account
  install_agent_binary
  install_version_file
  install_update_script
  write_environment_file
  install_service_file
  run_login
  set_agent_file_ownership
  install_update_sudoers
  start_service

  log "installation complete"
  log "logs: journalctl -u $SERVICE_NAME --no-pager -n 100"
}

main "$@"
