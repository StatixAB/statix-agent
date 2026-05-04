#!/usr/bin/env bash
set -Eeuo pipefail

readonly SERVICE_NAME="statix-agent"
readonly SERVICE_USER="statix-agent"
readonly SERVICE_GROUP="statix-agent"
readonly DEFAULT_DOWNLOAD_BASE_URL="https://github.com/statixab/statix-agent/releases/latest/download"
readonly UPDATE_SCRIPT_ASSET_NAME="statix-agent-update-ubuntu-24.04.sh"

DOWNLOAD_BASE_URL="${STATIX_DOWNLOAD_BASE_URL:-$DEFAULT_DOWNLOAD_BASE_URL}"
INSTALL_DIR="${STATIX_INSTALL_DIR:-/usr/local/bin}"
BINARY_PATH="${STATIX_BINARY_PATH:-$INSTALL_DIR/statix-agent}"
CONFIG_DIR="${STATIX_CONFIG_DIR:-/etc/statix}"
CONFIG_PATH="${STATIX_AGENT_CONFIG:-$CONFIG_DIR/agent.json}"
ENV_FILE="${STATIX_AGENT_ENV_FILE:-$CONFIG_DIR/agent.env}"
VERSION_FILE="${STATIX_VERSION_FILE:-/opt/statix/version.json}"
SERVICE_PATH="${STATIX_SERVICE_PATH:-/etc/systemd/system/$SERVICE_NAME.service}"
UPDATE_SCRIPT_PATH="${STATIX_UPDATE_SCRIPT_PATH:-/usr/local/lib/statix/update.sh}"
UPDATE_SERVICE_PATH="${STATIX_UPDATE_SERVICE_PATH:-/etc/systemd/system/$SERVICE_NAME-update.service}"
SUDOERS_PATH="${STATIX_AGENT_SUDOERS_PATH:-/etc/sudoers.d/$SERVICE_NAME}"
STATE_DIR="${STATIX_AGENT_STATE_DIR:-/var/lib/statix-agent}"
LXC_HOME="${STATIX_LXC_HOME:-$STATE_DIR/lxc}"
LXC_SUBID_START="${STATIX_LXC_SUBID_START:-100000}"
LXC_SUBID_COUNT="${STATIX_LXC_SUBID_COUNT:-65536}"
LXC_NETWORK_BRIDGE="${STATIX_LXC_NETWORK_BRIDGE:-lxcbr0}"
LXC_USERNET_DEVICES="${STATIX_LXC_USERNET_DEVICES:-10}"
API_BASE_URL="${STATIX_API_BASE_URL:-}"
NODE_NAME="${STATIX_NODE_NAME:-}"
SKIP_LOGIN="${STATIX_SKIP_LOGIN:-0}"
SKIP_START="${STATIX_SKIP_START:-0}"
MICROVM_IMAGE="${STATIX_MICROVM_IMAGE:-ubuntu-24.04}"
MICROVM_CPU="${STATIX_MICROVM_CPU:-2}"
MICROVM_MEMORY_MB="${STATIX_MICROVM_MEMORY_MB:-4096}"

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

check_proc_mount_options() {
  local proc_options
  proc_options="$(findmnt -no OPTIONS /proc 2>/dev/null || true)"

  if [[ ",$proc_options," == *",noatime,"* ]]; then
    log "warning: /proc is mounted with noatime; unprivileged LXC may fail to mount proc inside job containers"
    log "warning: remount /proc with relatime and update /etc/fstab before running container jobs"
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
    cloud-image-utils \
    iproute2 \
    lxc \
    pciutils \
    qemu-system-arm \
    qemu-system-x86 \
    qemu-utils \
    openssh-client \
    uidmap \
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

has_subid_allocation() {
  local file="$1"

  [[ -r "$file" ]] || return 1
  awk -F: -v user="$SERVICE_USER" '$1 == user { found = 1 } END { exit found ? 0 : 1 }' "$file"
}

subid_value() {
  local file="$1"
  local field="$2"

  awk -F: -v user="$SERVICE_USER" -v field="$field" '$1 == user { print $field; exit }' "$file"
}

ensure_lxc_subids() {
  local subid_end
  subid_end=$((LXC_SUBID_START + LXC_SUBID_COUNT - 1))

  if ! has_subid_allocation /etc/subuid; then
    log "allocating subuids $LXC_SUBID_START-$subid_end for $SERVICE_USER"
    usermod --add-subuids "$LXC_SUBID_START-$subid_end" "$SERVICE_USER"
  fi

  if ! has_subid_allocation /etc/subgid; then
    log "allocating subgids $LXC_SUBID_START-$subid_end for $SERVICE_USER"
    usermod --add-subgids "$LXC_SUBID_START-$subid_end" "$SERVICE_USER"
  fi
}

configure_lxc_runtime() {
  local lxc_config_dir lxc_config lxc_usernet_line uid_start uid_count gid_start gid_count
  lxc_config_dir="$LXC_HOME/.config/lxc"
  lxc_config="$lxc_config_dir/default.conf"
  lxc_usernet_line="$SERVICE_USER veth $LXC_NETWORK_BRIDGE $LXC_USERNET_DEVICES"

  ensure_lxc_subids
  uid_start="$(subid_value /etc/subuid 2)"
  uid_count="$(subid_value /etc/subuid 3)"
  gid_start="$(subid_value /etc/subgid 2)"
  gid_count="$(subid_value /etc/subgid 3)"

  [[ -n "$uid_start" && -n "$uid_count" ]] || fail "failed to determine subuid allocation for $SERVICE_USER"
  [[ -n "$gid_start" && -n "$gid_count" ]] || fail "failed to determine subgid allocation for $SERVICE_USER"

  install -d -m 0711 -o "$SERVICE_USER" -g "$SERVICE_GROUP" \
    "$STATE_DIR" \
    "$LXC_HOME" \
    "$LXC_HOME/containers"
  install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" \
    "$LXC_HOME/.cache" \
    "$LXC_HOME/.local" \
    "$LXC_HOME/.local/share" \
    "$lxc_config_dir"

  {
    if [[ -r /etc/lxc/default.conf ]]; then
      printf 'lxc.include = /etc/lxc/default.conf\n'
    else
      printf 'lxc.net.0.type = veth\n'
      printf 'lxc.net.0.link = %s\n' "$LXC_NETWORK_BRIDGE"
      printf 'lxc.net.0.flags = up\n'
    fi
    printf 'lxc.idmap = u 0 %s %s\n' "$uid_start" "$uid_count"
    printf 'lxc.idmap = g 0 %s %s\n' "$gid_start" "$gid_count"
    printf 'lxc.apparmor.profile = unconfined\n'
  } >"$lxc_config"
  chown "$SERVICE_USER:$SERVICE_GROUP" "$lxc_config"
  chmod 0640 "$lxc_config"

  install -d -m 0755 /etc/lxc
  touch /etc/lxc/lxc-usernet
  if ! grep -Fxq "$lxc_usernet_line" /etc/lxc/lxc-usernet; then
    log "allowing $SERVICE_USER to attach LXC veth devices to $LXC_NETWORK_BRIDGE"
    printf '%s\n' "$lxc_usernet_line" >>/etc/lxc/lxc-usernet
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
    printf 'STATIX_MICROVM_IMAGE=%s\n' "$MICROVM_IMAGE"
    printf 'STATIX_MICROVM_CPU=%s\n' "$MICROVM_CPU"
    printf 'STATIX_MICROVM_MEMORY_MB=%s\n' "$MICROVM_MEMORY_MB"
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
  check_proc_mount_options
  install_dependencies
  ensure_service_account
  configure_lxc_runtime
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
