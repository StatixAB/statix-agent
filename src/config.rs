use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_API_BASE_URL: &str = "https://statix.se/api";

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub node_id: String,
    pub node_token: String,
    pub agent_ws_url: String,
    pub api_base_url: String,
    pub publish_interval_ms: u64,
    pub system_info_check_interval_ms: u64,
    pub system_info_republish_interval_ms: u64,
    pub reconnect_delay_ms: u64,
    pub connect_timeout_ms: u64,
    pub wireguard: Option<WireGuardConfig>,
    pub container_default_image: String,
    pub container_default_cpu: u8,
    pub container_default_memory_mb: u32,
    pub microvm_default_image: String,
    pub microvm_default_cpu: u8,
    pub microvm_default_memory_mb: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedAgentConfig {
    pub node_id: String,
    pub node_token: String,
    pub agent_ws_url: String,
    pub api_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wireguard: Option<WireGuardConfig>,
}

#[derive(Debug, Clone)]
pub struct LoginConfig {
    pub api_base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireGuardConfig {
    #[serde(rename = "interfaceName")]
    pub interface_name: String,
    pub addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<String>,
    #[serde(rename = "privateKey")]
    pub private_key: String,
    #[serde(rename = "publicKey")]
    pub public_key: String,
    pub server: WireGuardPeerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireGuardPeerConfig {
    #[serde(rename = "publicKey")]
    pub public_key: String,
    pub endpoint: String,
    #[serde(rename = "allowedIps")]
    pub allowed_ips: Vec<String>,
    #[serde(
        rename = "presharedKey",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub preshared_key: Option<String>,
    #[serde(
        rename = "persistentKeepaliveSeconds",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub persistent_keepalive_seconds: Option<u16>,
}

impl AgentConfig {
    pub fn load() -> Result<Option<Self>> {
        let persisted = load_persisted_config()?;

        let node_id = env::var("NODE_ID")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| persisted.as_ref().map(|value| value.node_id.clone()));
        let Some(node_id) = node_id else {
            return Ok(None);
        };
        let node_token = env::var("NODE_TOKEN")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| persisted.as_ref().map(|value| value.node_token.clone()));
        let Some(node_token) = node_token else {
            return Ok(None);
        };
        let agent_ws_url = env::var("AGENT_WS_URL")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                env::var("API_BASE_URL")
                    .ok()
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty())
                    .map(|value| {
                        let ws_path =
                            env::var("NODE_WS_PATH").unwrap_or_else(|_| "/api/ws/agent".to_owned());
                        format!(
                            "{}{}",
                            to_ws_base_url(&trim_trailing_slash(&value)),
                            normalize_path(&ws_path)
                        )
                    })
            })
            .or_else(|| persisted.as_ref().map(|value| value.agent_ws_url.clone()));
        let Some(agent_ws_url) = agent_ws_url else {
            return Ok(None);
        };
        let api_base_url = normalize_api_base_url(
            &env::var("API_BASE_URL")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    persisted
                        .as_ref()
                        .and_then(|value| value.api_base_url.clone())
                })
                .unwrap_or_else(|| api_base_url_from_ws_url(&agent_ws_url)),
        );

        Ok(Some(Self {
            node_id,
            node_token,
            agent_ws_url,
            api_base_url,
            publish_interval_ms: parse_positive_int("PUBLISH_INTERVAL_MS", 5_000),
            system_info_check_interval_ms: parse_positive_int(
                "SYSTEM_INFO_CHECK_INTERVAL_MS",
                10 * 60_000,
            ),
            system_info_republish_interval_ms: parse_positive_int(
                "SYSTEM_INFO_REPUBLISH_INTERVAL_MS",
                24 * 60 * 60_000,
            ),
            reconnect_delay_ms: parse_positive_int("RECONNECT_DELAY_MS", 3_000),
            connect_timeout_ms: parse_positive_int("WS_CONNECT_TIMEOUT_MS", 8_000),
            wireguard: resolve_wireguard_config(persisted.as_ref()),
            container_default_image: env::var("STATIX_CONTAINER_IMAGE")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "ubuntu:24.04".to_string()),
            container_default_cpu: parse_positive_u8("STATIX_CONTAINER_CPU", 2),
            container_default_memory_mb: parse_positive_u32("STATIX_CONTAINER_MEMORY_MB", 4096),
            microvm_default_image: env::var("STATIX_MICROVM_IMAGE")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "ubuntu-24.04".to_string()),
            microvm_default_cpu: parse_positive_u8("STATIX_MICROVM_CPU", 2),
            microvm_default_memory_mb: parse_positive_u32("STATIX_MICROVM_MEMORY_MB", 4096),
        }))
    }
}

pub fn resolve_login_config(explicit_api_base_url: Option<String>) -> LoginConfig {
    let persisted = load_persisted_config().ok().flatten();
    LoginConfig {
        api_base_url: resolve_login_api_base_url(
            explicit_api_base_url,
            env::var("API_BASE_URL").ok(),
            persisted.and_then(|value| value.api_base_url),
        ),
    }
}

pub fn load_persisted_config_snapshot() -> Result<Option<PersistedAgentConfig>> {
    load_persisted_config()
}

pub fn save_persisted_config(config: &PersistedAgentConfig) -> Result<PathBuf> {
    let path = persisted_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let payload = serde_json::to_string_pretty(config)?;
    fs::write(&path, payload).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

pub fn agent_state_dir() -> Result<PathBuf> {
    if let Some(path) = env_path("STATIX_AGENT_STATE_DIR") {
        return Ok(path);
    }

    // systemd services with StateDirectory set expose this variable at runtime.
    if let Some(path) = env_path("STATE_DIRECTORY") {
        return Ok(path);
    }

    if let Some(path) = env_path("XDG_STATE_HOME") {
        return Ok(path.join("statix"));
    }

    let path = persisted_config_path()?;
    if let Some(parent) = path.parent() {
        return Ok(parent.to_path_buf());
    }

    env::current_dir().context("failed to resolve current directory")
}

fn load_persisted_config() -> Result<Option<PersistedAgentConfig>> {
    let path = persisted_config_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let parsed = serde_json::from_str::<PersistedAgentConfig>(&raw)
        .with_context(|| format!("invalid {}", path.display()))?;
    Ok(Some(parsed))
}

fn persisted_config_path() -> Result<PathBuf> {
    if let Ok(value) = env::var("STATIX_AGENT_CONFIG") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    if let Ok(value) = env::var("XDG_CONFIG_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(Path::new(trimmed).join("statix").join("agent.json"));
        }
    }

    if let Ok(value) = env::var("APPDATA") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(Path::new(trimmed).join("statix").join("agent.json"));
        }
    }

    if let Ok(value) = env::var("HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(Path::new(trimmed)
                .join(".config")
                .join("statix")
                .join("agent.json"));
        }
    }

    let cwd = env::current_dir().context("failed to resolve current directory")?;
    Ok(cwd.join(".statix-agent.json"))
}

fn parse_positive_int(name: &str, fallback: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

fn parse_positive_u8(name: &str, fallback: u8) -> u8 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u8>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

fn parse_positive_u32(name: &str, fallback: u32) -> u32 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn resolve_wireguard_config(persisted: Option<&PersistedAgentConfig>) -> Option<WireGuardConfig> {
    if env_flag("WIREGUARD_DISABLE") {
        return None;
    }

    let interface_name = env::var("WIREGUARD_INTERFACE_NAME")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let addresses = parse_csv_env("WIREGUARD_ADDRESSES");
    let dns = parse_csv_env("WIREGUARD_DNS");
    let private_key = env::var("WIREGUARD_PRIVATE_KEY")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let public_key = env::var("WIREGUARD_PUBLIC_KEY")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let server_public_key = env::var("WIREGUARD_SERVER_PUBLIC_KEY")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let server_endpoint = env::var("WIREGUARD_SERVER_ENDPOINT")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let allowed_ips = parse_csv_env("WIREGUARD_ALLOWED_IPS");
    let preshared_key = env::var("WIREGUARD_PRESHARED_KEY")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let persistent_keepalive_seconds = env::var("WIREGUARD_PERSISTENT_KEEPALIVE_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|value| *value > 0);

    let env_complete = interface_name.is_some()
        || !addresses.is_empty()
        || !dns.is_empty()
        || private_key.is_some()
        || public_key.is_some()
        || server_public_key.is_some()
        || server_endpoint.is_some()
        || !allowed_ips.is_empty()
        || preshared_key.is_some()
        || persistent_keepalive_seconds.is_some();

    if env_complete {
        let persisted_wireguard = persisted.and_then(|value| value.wireguard.as_ref());
        let interface_name = interface_name
            .or_else(|| persisted_wireguard.map(|value| value.interface_name.clone()))?;
        let private_key =
            private_key.or_else(|| persisted_wireguard.map(|value| value.private_key.clone()))?;
        let public_key =
            public_key.or_else(|| persisted_wireguard.map(|value| value.public_key.clone()))?;
        let server_public_key = server_public_key
            .or_else(|| persisted_wireguard.map(|value| value.server.public_key.clone()))?;
        let server_endpoint = server_endpoint
            .or_else(|| persisted_wireguard.map(|value| value.server.endpoint.clone()))?;
        let addresses = if addresses.is_empty() {
            persisted_wireguard
                .map(|value| value.addresses.clone())
                .unwrap_or_default()
        } else {
            addresses
        };
        let dns = if dns.is_empty() {
            persisted_wireguard
                .map(|value| value.dns.clone())
                .unwrap_or_default()
        } else {
            dns
        };
        let allowed_ips = if allowed_ips.is_empty() {
            persisted_wireguard
                .map(|value| value.server.allowed_ips.clone())
                .unwrap_or_default()
        } else {
            allowed_ips
        };
        if addresses.is_empty() || allowed_ips.is_empty() {
            return None;
        }

        return Some(WireGuardConfig {
            interface_name,
            addresses,
            dns,
            private_key,
            public_key,
            server: WireGuardPeerConfig {
                public_key: server_public_key,
                endpoint: server_endpoint,
                allowed_ips,
                preshared_key: preshared_key.or_else(|| {
                    persisted_wireguard.and_then(|value| value.server.preshared_key.clone())
                }),
                persistent_keepalive_seconds: persistent_keepalive_seconds.or_else(|| {
                    persisted_wireguard.and_then(|value| value.server.persistent_keepalive_seconds)
                }),
            },
        });
    }

    persisted.and_then(|value| value.wireguard.clone())
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn parse_csv_env(name: &str) -> Vec<String> {
    env::var(name)
        .ok()
        .map(|value| parse_csv(&value))
        .unwrap_or_default()
}

fn parse_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn trim_trailing_slash(value: &str) -> String {
    value.trim_end_matches('/').to_owned()
}

fn normalize_api_base_url(value: &str) -> String {
    let trimmed = trim_trailing_slash(value.trim());
    if trimmed.is_empty() {
        return String::new();
    }

    let Some(_hostname) = extract_hostname(&trimmed) else {
        return trimmed;
    };

    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, remainder)| remainder)
        .unwrap_or(trimmed.as_str());
    let path = without_scheme
        .find('/')
        .map(|index| &without_scheme[index..])
        .unwrap_or("");

    if path.is_empty() {
        return format!("{trimmed}/api");
    }

    trimmed
}

fn resolve_login_api_base_url(
    explicit_api_base_url: Option<String>,
    env_api_base_url: Option<String>,
    persisted_api_base_url: Option<String>,
) -> String {
    let api_base_url = explicit_api_base_url
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            env_api_base_url
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            persisted_api_base_url
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .filter(|value| !is_loopback_like_url(value))
        })
        .unwrap_or_else(|| DEFAULT_API_BASE_URL.to_owned());

    normalize_api_base_url(&api_base_url)
}

fn is_loopback_like_url(value: &str) -> bool {
    let hostname = match extract_hostname(value) {
        Some(hostname) => hostname,
        None => return false,
    };

    matches!(hostname.as_str(), "localhost" | "127.0.0.1" | "::1") || hostname.ends_with(".nip.io")
}

fn extract_hostname(value: &str) -> Option<String> {
    let without_scheme = value
        .split_once("://")
        .map(|(_, remainder)| remainder)
        .unwrap_or(value);
    let authority = without_scheme.split('/').next()?.trim();
    if authority.is_empty() {
        return None;
    }

    if authority.starts_with('[') {
        let end = authority.find(']')?;
        return Some(authority[1..end].to_ascii_lowercase());
    }

    let host = authority.split(':').next()?.trim();
    if host.is_empty() {
        return None;
    }

    Some(host.to_ascii_lowercase())
}

fn normalize_path(value: &str) -> String {
    if value.starts_with('/') {
        value.to_owned()
    } else {
        format!("/{value}")
    }
}

fn to_ws_base_url(value: &str) -> String {
    if let Some(suffix) = value.strip_prefix("https://") {
        return format!("wss://{suffix}");
    }
    if let Some(suffix) = value.strip_prefix("http://") {
        return format!("ws://{suffix}");
    }
    if value.starts_with("ws://") || value.starts_with("wss://") {
        return value.to_owned();
    }

    format!("ws://{value}")
}

fn api_base_url_from_ws_url(value: &str) -> String {
    let base = if let Some(suffix) = value.strip_prefix("wss://") {
        format!("https://{suffix}")
    } else if let Some(suffix) = value.strip_prefix("ws://") {
        format!("http://{suffix}")
    } else {
        value.to_owned()
    };

    if let Some(stripped) = base.strip_suffix("/api/ws/agent") {
        return stripped.to_owned();
    }
    if let Some(stripped) = base.strip_suffix("/ws/agent") {
        return normalize_api_base_url(stripped);
    }

    normalize_api_base_url(&base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_login_config_uses_hosted_default_when_no_source_exists() {
        let api_base_url = resolve_login_api_base_url(None, None, None);
        assert_eq!(api_base_url, DEFAULT_API_BASE_URL);
    }

    #[test]
    fn resolve_login_config_prefers_explicit_value() {
        let api_base_url = resolve_login_api_base_url(
            Some(" http://example.com:3001/path/ ".to_owned()),
            Some("http://env:3001".to_owned()),
            Some("http://persisted:3001".to_owned()),
        );
        assert_eq!(api_base_url, "http://example.com:3001/path");
    }

    #[test]
    fn resolve_login_config_prefers_env_over_persisted() {
        let api_base_url = resolve_login_api_base_url(
            None,
            Some("http://env:3001/".to_owned()),
            Some("http://persisted:3001".to_owned()),
        );
        assert_eq!(api_base_url, "http://env:3001/api");
    }

    #[test]
    fn resolve_login_config_ignores_persisted_loopback_url() {
        let api_base_url = resolve_login_api_base_url(
            None,
            None,
            Some("http://statix.127.0.0.1.nip.io".to_owned()),
        );
        assert_eq!(api_base_url, DEFAULT_API_BASE_URL);
    }

    #[test]
    fn resolve_login_config_keeps_persisted_non_loopback_url() {
        let api_base_url = resolve_login_api_base_url(
            None,
            None,
            Some("https://selfhosted.example.com/".to_owned()),
        );
        assert_eq!(api_base_url, "https://selfhosted.example.com/api");
    }

    #[test]
    fn resolve_login_config_scopes_hosted_root_url_to_api() {
        let api_base_url =
            resolve_login_api_base_url(Some("https://statix.se".to_owned()), None, None);
        assert_eq!(api_base_url, "https://statix.se/api");
    }

    #[test]
    fn normalize_api_base_url_scopes_hosted_subdomain_root_url_to_api() {
        let api_base_url = normalize_api_base_url("https://dev.statix.se/");
        assert_eq!(api_base_url, "https://dev.statix.se/api");
    }

    #[test]
    fn normalize_api_base_url_keeps_api_path() {
        let api_base_url = normalize_api_base_url("https://dev.statix.se/api/");
        assert_eq!(api_base_url, "https://dev.statix.se/api");
    }

    #[test]
    fn resolve_login_config_scopes_local_root_url_to_api() {
        let api_base_url =
            resolve_login_api_base_url(Some("http://localhost:3001/".to_owned()), None, None);
        assert_eq!(api_base_url, "http://localhost:3001/api");
    }
}
