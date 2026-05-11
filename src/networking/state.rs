use std::{collections::BTreeMap, fs, net::Ipv4Addr, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::agent_state_dir;

use super::ExposureVisibility;

const STATE_VERSION: u32 = 1;
const DEFAULT_BRIDGE: &str = "statix0";
const DEFAULT_SUBNET_PREFIX: u8 = 16;
pub(super) const DEFAULT_GATEWAY: Ipv4Addr = Ipv4Addr::new(172, 31, 0, 1);

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct AgentNetworkState {
    pub(super) version: u32,
    pub(super) bridge: BridgeState,
    #[serde(default)]
    pub(super) projects: BTreeMap<String, ProjectNetworkState>,
    #[serde(default)]
    pub(super) allocated_host_ports: BTreeMap<u16, HostPortAllocation>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct BridgeState {
    pub(super) name: String,
    pub(super) gateway: Ipv4Addr,
    pub(super) prefix_len: u8,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ProjectNetworkState {
    pub(super) project_id: String,
    pub(super) environment: String,
    pub(super) vm_id: String,
    pub(super) vm_ip: Ipv4Addr,
    pub(super) tap_name: String,
    pub(super) mac_address: String,
    #[serde(default)]
    pub(super) reserved: bool,
    #[serde(default)]
    pub(super) exposures: Vec<ExposureState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ExposureState {
    pub(super) host: String,
    pub(super) target_port: u16,
    pub(super) visibility: ExposureVisibility,
    pub(super) status: ExposureStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) reason: Option<String>,
    pub(super) owner: RuleOwner,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ExposureStatus {
    Active,
    Denied,
    Pending,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct HostPortAllocation {
    pub(super) project_key: String,
    pub(super) port: u16,
    pub(super) protocol: String,
    pub(super) owner: RuleOwner,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RuleOwner {
    pub(super) owner: String,
}

impl RuleOwner {
    pub(super) fn statix() -> Self {
        Self {
            owner: "statix-agent".to_string(),
        }
    }
}

pub(super) fn load_state() -> Result<AgentNetworkState> {
    let path = state_path()?;
    if !path.exists() {
        return Ok(AgentNetworkState {
            version: STATE_VERSION,
            bridge: BridgeState {
                name: DEFAULT_BRIDGE.to_string(),
                gateway: DEFAULT_GATEWAY,
                prefix_len: DEFAULT_SUBNET_PREFIX,
            },
            projects: BTreeMap::new(),
            allocated_host_ports: BTreeMap::new(),
        });
    }
    let data =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

pub(super) fn save_state(state: &mut AgentNetworkState) -> Result<()> {
    state.version = STATE_VERSION;
    let path = state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(state)?;
    fs::write(&path, data).with_context(|| format!("failed to write {}", path.display()))
}

pub(super) fn proxy_dir() -> Result<PathBuf> {
    Ok(agent_state_dir()?.join("networking").join("proxy"))
}

fn state_path() -> Result<PathBuf> {
    Ok(agent_state_dir()?.join("networking").join("state.json"))
}
