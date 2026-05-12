use std::{
    collections::{BTreeSet, HashSet},
    env, fs,
    net::Ipv4Addr,
    path::Path,
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::logs;

mod commands;
mod state;

use commands::{command_output, delete_link_best_effort, link_exists, run_ip};
use state::{
    AgentNetworkState, BridgeState, DEFAULT_GATEWAY, ExposureState, ExposureStatus,
    HostPortAllocation, ProjectNetworkState, RuleOwner, load_state, proxy_dir, save_state,
};

const DYNAMIC_PORT_START: u16 = 30_000;
const DYNAMIC_PORT_END: u16 = 39_999;

// Ports statix will never touch
const RESERVED_PORTS: &[u16] = &[22, 53, 80, 443, 2375, 2376, 5432, 3306, 6379, 6443, 10250];

#[derive(Debug, Clone)]
pub struct VmNetworkLease {
    pub vm_ip: Ipv4Addr,
    pub tap_name: String,
    pub mac_address: String,
    pub prefix_len: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExposureRequest {
    #[serde(default)]
    pub host: Option<String>,
    pub target_port: u16,
    #[serde(default = "default_visibility")]
    pub visibility: ExposureVisibility,
    #[serde(default)]
    pub workspace_public_http_allowed: bool,
    #[serde(default)]
    pub node_ingress_allowed: bool,
    #[serde(default = "default_project_exposure_allowed")]
    pub project_exposure_allowed: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExposureVisibility {
    Internal,
    Private,
    Public,
}

impl ExposureVisibility {
    fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Private => "private",
            Self::Public => "public",
        }
    }
}

pub fn reconcile_on_start() -> Result<()> {
    let mut state = load_state()?;
    ensure_bridge(&state.bridge)?;
    ensure_managed_exposure_ports(&mut state)?;
    render_proxy_routes(&state)?;
    save_state(&mut state)
}

pub fn ensure_project_network(
    project_id: &str,
    environment: &str,
    vm_id: &str,
    exposures: &[ExposureRequest],
) -> Result<VmNetworkLease> {
    let mut state = load_state()?;
    ensure_bridge(&state.bridge)?;
    let key = project_key(project_id, environment);
    if !state.projects.contains_key(&key) {
        let vm_ip = allocate_vm_ip(&state, &key)?;
        state.projects.insert(
            key.clone(),
            ProjectNetworkState {
                project_id: project_id.to_string(),
                environment: environment.to_string(),
                vm_id: vm_id.to_string(),
                vm_ip,
                tap_name: tap_name_for(&key),
                mac_address: mac_address_for(&key),
                reserved: false,
                exposures: Vec::new(),
            },
        );
    }
    let project = state
        .projects
        .get(&key)
        .ok_or_else(|| anyhow!("failed to reserve project network state"))?;
    let mut project_exposures = exposures
        .iter()
        .map(|request| evaluate_exposure(project, request))
        .collect::<Result<Vec<_>>>()?;
    let mut active_managed_ports = HashSet::new();
    for exposure in &mut project_exposures {
        if exposure.status == ExposureStatus::Active
            && exposure.visibility != ExposureVisibility::Public
        {
            let host_port =
                allocate_managed_port_for(&mut state, &key, "tcp", exposure.target_port)?;
            exposure.host_port = Some(host_port);
            active_managed_ports.insert(("tcp".to_string(), exposure.target_port));
        }
    }
    release_unused_host_ports(&mut state, &key, &active_managed_ports);

    let project = state
        .projects
        .get_mut(&key)
        .ok_or_else(|| anyhow!("failed to reserve project network state"))?;
    project.vm_id = vm_id.to_string();
    project.reserved = false;
    project.exposures = project_exposures;

    let lease = VmNetworkLease {
        vm_ip: project.vm_ip,
        tap_name: project.tap_name.clone(),
        mac_address: project.mac_address.clone(),
        prefix_len: state.bridge.prefix_len,
    };
    ensure_tap(&state.bridge, &lease.tap_name)?;
    render_proxy_routes(&state)?;
    save_state(&mut state)?;
    Ok(lease)
}

pub fn stop_project_network(project_id: &str, environment: &str, reserve_ip: bool) -> Result<()> {
    let mut state = load_state()?;
    let key = project_key(project_id, environment);
    if let Some(project) = state.projects.get_mut(&key) {
        delete_tap(&project.tap_name);
        project.exposures.clear();
        if reserve_ip {
            project.reserved = true;
        } else {
            state.projects.remove(&key);
        }
    }
    render_proxy_routes(&state)?;
    save_state(&mut state)
}

pub fn project_route_descriptions(project_id: &str, environment: &str) -> Result<Vec<String>> {
    let state = load_state()?;
    let key = project_key(project_id, environment);
    let Some(project) = state.projects.get(&key) else {
        return Ok(Vec::new());
    };

    Ok(project
        .exposures
        .iter()
        .map(|exposure| {
            if exposure.status == ExposureStatus::Active {
                let bind = exposure_bind(exposure);
                format!(
                    "{} exposure: {} -> {}:{}",
                    exposure.visibility.as_str(),
                    bind,
                    project.vm_ip,
                    exposure.target_port
                )
            } else {
                format!(
                    "{} exposure for {}:{} is {}{}",
                    exposure.visibility.as_str(),
                    project.vm_ip,
                    exposure.target_port,
                    exposure.status.as_str(),
                    exposure
                        .reason
                        .as_ref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                )
            }
        })
        .collect())
}

#[allow(dead_code)]
pub fn allocate_managed_port(project_id: &str, environment: &str, protocol: &str) -> Result<u16> {
    let mut state = load_state()?;
    let blocked = blocked_ports(&state);
    let project_key = project_key(project_id, environment);
    for port in DYNAMIC_PORT_START..=DYNAMIC_PORT_END {
        if blocked.contains(&port) {
            continue;
        }
        state.allocated_host_ports.insert(
            port,
            HostPortAllocation {
                project_key,
                port,
                protocol: protocol.to_string(),
                target_port: 0,
                owner: RuleOwner::statix(),
            },
        );
        save_state(&mut state)?;
        return Ok(port);
    }
    bail!("no free managed host ports in {DYNAMIC_PORT_START}-{DYNAMIC_PORT_END}")
}

fn evaluate_exposure(
    project: &ProjectNetworkState,
    request: &ExposureRequest,
) -> Result<ExposureState> {
    if request.target_port == 0 {
        bail!("exposure targetPort must be between 1 and 65535");
    }
    let host = request
        .host
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}.statix.local", project.project_id));
    let host_missing = request
        .host
        .as_ref()
        .is_none_or(|value| value.trim().is_empty());
    let pending = request.visibility == ExposureVisibility::Public && host_missing;

    let denied_reason = match request.visibility {
        ExposureVisibility::Internal | ExposureVisibility::Private => None,
        ExposureVisibility::Public if !request.workspace_public_http_allowed => {
            Some("workspace policy does not allow public HTTP exposure")
        }
        ExposureVisibility::Public if !request.node_ingress_allowed => {
            Some("node is not allowed to act as an ingress node")
        }
        ExposureVisibility::Public if !request.project_exposure_allowed => {
            Some("project is not allowed to request public exposure")
        }
        ExposureVisibility::Public => None,
    };

    Ok(ExposureState {
        host,
        target_port: request.target_port,
        host_port: None,
        visibility: request.visibility,
        status: if pending {
            ExposureStatus::Pending
        } else if denied_reason.is_some() {
            ExposureStatus::Denied
        } else {
            ExposureStatus::Active
        },
        reason: if pending {
            Some("exposure host is not assigned yet".to_string())
        } else {
            denied_reason.map(str::to_string)
        },
        owner: RuleOwner::statix(),
    })
}

fn ensure_bridge(bridge: &BridgeState) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("statix microVM bridge networking is supported on Linux only");
    }
    if !link_exists(&bridge.name) {
        run_ip(&["link", "add", "name", &bridge.name, "type", "bridge"])?;
    }
    let cidr = format!("{}/{}", bridge.gateway, bridge.prefix_len);
    let addresses = command_output("ip", &["-4", "-o", "addr", "show", "dev", &bridge.name])
        .unwrap_or_default();
    if !addresses.contains(&cidr) {
        run_ip(&["addr", "replace", &cidr, "dev", &bridge.name])?;
    }
    run_ip(&["link", "set", "dev", &bridge.name, "up"])
}

fn ensure_tap(bridge: &BridgeState, tap_name: &str) -> Result<()> {
    if !link_exists(tap_name) {
        let user = env::var("USER").unwrap_or_else(|_| "statix-agent".to_string());
        run_ip(&[
            "tuntap", "add", "dev", tap_name, "mode", "tap", "user", &user,
        ])?;
    }
    run_ip(&["link", "set", tap_name, "master", &bridge.name])?;
    run_ip(&["link", "set", "dev", tap_name, "up"])
}

fn delete_tap(tap_name: &str) {
    if link_exists(tap_name) {
        delete_link_best_effort(tap_name);
    }
}

fn render_proxy_routes(state: &AgentNetworkState) -> Result<()> {
    let dir = proxy_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let caddyfile = dir.join("Caddyfile");
    let mut routes = String::from("# Generated by statix-agent. Do not edit by hand.\n\n");
    let mut active_routes = 0usize;
    for project in state.projects.values() {
        for exposure in &project.exposures {
            if exposure.status == ExposureStatus::Active {
                active_routes += 1;
                let bind = exposure_bind(exposure);
                routes.push_str(&format!(
                    "{} {{\n\treverse_proxy {}:{}\n}}\n\n",
                    bind, project.vm_ip, exposure.target_port
                ));
            }
        }
    }
    fs::write(&caddyfile, routes)
        .with_context(|| format!("failed to write {}", caddyfile.display()))?;
    reload_caddy_best_effort(&caddyfile, active_routes > 0);
    Ok(())
}

fn exposure_bind(exposure: &ExposureState) -> String {
    if exposure.visibility == ExposureVisibility::Public {
        exposure.host.clone()
    } else {
        format!(
            "127.0.0.1:{}",
            exposure.host_port.unwrap_or(exposure.target_port)
        )
    }
}

fn allocate_managed_port_for(
    state: &mut AgentNetworkState,
    project_key: &str,
    protocol: &str,
    target_port: u16,
) -> Result<u16> {
    if let Some((port, _)) = state.allocated_host_ports.iter().find(|(_, allocation)| {
        allocation.project_key == project_key
            && allocation.protocol == protocol
            && allocation.target_port == target_port
    }) {
        return Ok(*port);
    }

    let blocked = blocked_ports(state);
    for port in DYNAMIC_PORT_START..=DYNAMIC_PORT_END {
        if blocked.contains(&port) {
            continue;
        }
        state.allocated_host_ports.insert(
            port,
            HostPortAllocation {
                project_key: project_key.to_string(),
                port,
                protocol: protocol.to_string(),
                target_port,
                owner: RuleOwner::statix(),
            },
        );
        return Ok(port);
    }
    bail!("no free managed host ports in {DYNAMIC_PORT_START}-{DYNAMIC_PORT_END}")
}

fn ensure_managed_exposure_ports(state: &mut AgentNetworkState) -> Result<()> {
    let project_keys = state.projects.keys().cloned().collect::<Vec<_>>();

    for project_key in project_keys {
        let target_ports = state
            .projects
            .get(&project_key)
            .map(|project| {
                project
                    .exposures
                    .iter()
                    .enumerate()
                    .filter_map(|(index, exposure)| {
                        if exposure.status == ExposureStatus::Active
                            && exposure.visibility != ExposureVisibility::Public
                        {
                            Some((index, exposure.target_port))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut active_managed_ports = HashSet::new();
        for (index, target_port) in target_ports {
            let host_port = allocate_managed_port_for(state, &project_key, "tcp", target_port)?;
            if let Some(project) = state.projects.get_mut(&project_key) {
                if let Some(exposure) = project.exposures.get_mut(index) {
                    exposure.host_port = Some(host_port);
                }
            }
            active_managed_ports.insert(("tcp".to_string(), target_port));
        }
        release_unused_host_ports(state, &project_key, &active_managed_ports);
    }

    Ok(())
}

fn release_unused_host_ports(
    state: &mut AgentNetworkState,
    project_key: &str,
    active_ports: &HashSet<(String, u16)>,
) {
    state.allocated_host_ports.retain(|_, allocation| {
        allocation.project_key != project_key
            || active_ports.contains(&(allocation.protocol.clone(), allocation.target_port))
    });
}

fn reload_caddy_best_effort(caddyfile: &Path, start_if_stopped: bool) {
    match Command::new("caddy")
        .arg("validate")
        .arg("--config")
        .arg(caddyfile)
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            logs::agent_warn(format!(
                "caddy config validation failed with {status}; exposure proxy routes were not reloaded"
            ));
            return;
        }
        Err(error) => {
            logs::agent_warn(format!(
                "failed to launch caddy for exposure proxy routes: {error}"
            ));
            return;
        }
    }

    match Command::new("caddy")
        .arg("reload")
        .arg("--config")
        .arg(caddyfile)
        .status()
    {
        Ok(status) if status.success() => return,
        Ok(status) => logs::agent_warn(format!(
            "caddy reload failed with {status}; attempting to start caddy"
        )),
        Err(error) => {
            logs::agent_warn(format!(
                "failed to launch caddy reload for exposure proxy routes: {error}"
            ));
            return;
        }
    }

    if !start_if_stopped {
        return;
    }

    if let Err(error) = Command::new("caddy")
        .arg("start")
        .arg("--config")
        .arg(caddyfile)
        .status()
    {
        logs::agent_warn(format!(
            "failed to launch caddy start for exposure proxy routes: {error}"
        ));
    }
}

fn allocate_vm_ip(state: &AgentNetworkState, key: &str) -> Result<Ipv4Addr> {
    let used = state
        .projects
        .values()
        .map(|project| project.vm_ip)
        .collect::<BTreeSet<_>>();
    let mut seed = Sha256::new();
    seed.update(key.as_bytes());
    let digest = seed.finalize();
    let start = u16::from_be_bytes([digest[0], digest[1]]) % 60_000;
    for offset in 0..60_000u16 {
        let host = 2 + ((start + offset) % 60_000);
        let ip = Ipv4Addr::new(172, 31, (host / 256) as u8, (host % 256) as u8);
        if ip != DEFAULT_GATEWAY && !used.contains(&ip) {
            return Ok(ip);
        }
    }
    bail!("no available addresses in 172.31.0.0/16")
}

#[allow(dead_code)]
fn blocked_ports(state: &AgentNetworkState) -> BTreeSet<u16> {
    let mut blocked = RESERVED_PORTS.iter().copied().collect::<BTreeSet<_>>();
    blocked.extend(state.allocated_host_ports.keys().copied());
    blocked.extend(listening_ports().unwrap_or_default());
    blocked.extend(firewall_ports().unwrap_or_default());
    blocked
}

#[allow(dead_code)]
fn listening_ports() -> Result<BTreeSet<u16>> {
    let mut ports = BTreeSet::new();
    for args in [["-H", "-ltn"].as_slice(), ["-H", "-lun"].as_slice()] {
        let output = command_output("ss", args)?;
        for line in output.lines() {
            if let Some(port) = line.split_whitespace().nth(4).and_then(parse_port) {
                ports.insert(port);
            }
        }
    }
    Ok(ports)
}

#[allow(dead_code)]
fn parse_port(value: &str) -> Option<u16> {
    value.rsplit_once(':')?.1.parse().ok()
}

#[allow(dead_code)]
fn firewall_ports() -> Result<BTreeSet<u16>> {
    let mut ports = BTreeSet::new();
    if let Ok(output) = command_output("nft", &["list", "ruleset"]) {
        ports.extend(parse_firewall_ports(&output));
    }
    if let Ok(output) = command_output("iptables-save", &[]) {
        ports.extend(parse_firewall_ports(&output));
    }
    Ok(ports)
}

#[allow(dead_code)]
fn parse_firewall_ports(output: &str) -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();
    let mut previous = "";
    for token in output.split_whitespace() {
        let trimmed = token
            .trim_matches(',')
            .trim_matches('{')
            .trim_matches('}')
            .trim_matches('"');
        if matches!(previous, "dport" | "--dport" | "--to-port" | "--to-ports") {
            collect_port_token(trimmed, &mut ports);
        }
        if let Some((_, port)) = trimmed.rsplit_once(':') {
            collect_port_token(port, &mut ports);
        }
        previous = trimmed;
    }
    ports
}

#[allow(dead_code)]
fn collect_port_token(value: &str, ports: &mut BTreeSet<u16>) {
    for segment in value.split(',') {
        let first = segment.split('-').next().unwrap_or(segment);
        if let Ok(port) = first.parse::<u16>() {
            ports.insert(port);
        }
    }
}

fn project_key(project_id: &str, environment: &str) -> String {
    format!("{}-{}", safe_segment(project_id), safe_segment(environment))
}

fn tap_name_for(key: &str) -> String {
    format!("stx{}", short_hash(key, 11))
}

fn mac_address_for(key: &str) -> String {
    let hash = short_hash(key, 10);
    format!(
        "02:53:{}:{}:{}:{}",
        &hash[0..2],
        &hash[2..4],
        &hash[4..6],
        &hash[6..8]
    )
}

fn short_hash(value: &str, len: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())[..len].to_string()
}

fn safe_segment(value: &str) -> String {
    let mut out = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "default".to_string()
    } else {
        out
    }
}

fn default_visibility() -> ExposureVisibility {
    ExposureVisibility::Private
}

fn default_project_exposure_allowed() -> bool {
    true
}
