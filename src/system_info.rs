use std::{env, fs, path::PathBuf, sync::OnceLock};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sysinfo::System;
use tokio::{process::Command, time::timeout};

use crate::{
    config::WireGuardConfig,
    wireguard::{WireGuardStatus, collect_status as collect_wireguard_status},
};

#[derive(Debug, Serialize)]
pub struct SystemInfoPayload {
    pub v: u8,
    pub ts: u64,
    pub hash: String,
    pub info: SystemInfo,
}

#[derive(Debug, Serialize)]
pub struct SystemInfo {
    #[serde(rename = "osPlatform")]
    pub os_platform: String,
    #[serde(rename = "osRelease")]
    pub os_release: String,
    #[serde(rename = "osArch")]
    pub os_arch: String,
    pub hostname: String,
    #[serde(rename = "cpuModel")]
    pub cpu_model: String,
    #[serde(rename = "cpuCores")]
    pub cpu_cores: u32,
    #[serde(rename = "memTotal")]
    pub mem_total: u64,
    #[serde(rename = "agentVersion", skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(rename = "agentCommit", skip_serializing_if = "Option::is_none")]
    pub agent_commit: Option<String>,
    #[serde(rename = "agentBuiltAt", skip_serializing_if = "Option::is_none")]
    pub agent_built_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wireguard: Option<WireGuardStatus>,
    pub gpus: Vec<GpuInfo>,
}

#[derive(Debug, Serialize)]
pub struct GpuInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(rename = "memoryBytes", skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(rename = "driverVersion", skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct VersionMetadata {
    version: String,
    commit: Option<String>,
    #[serde(rename = "builtAt")]
    built_at: Option<String>,
}

static VERSION_METADATA: OnceLock<Option<VersionMetadata>> = OnceLock::new();

pub fn collect_mac_addresses() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        let entries = match fs::read_dir("/sys/class/net") {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };

        let mut mac_addresses = Vec::new();
        for entry in entries.flatten() {
            let interface_name = entry.file_name();
            if interface_name.to_string_lossy() == "lo" {
                continue;
            }

            let address_path = entry.path().join("address");
            let raw = match fs::read_to_string(address_path) {
                Ok(raw) => raw,
                Err(_) => continue,
            };

            let Some(mac_address) = normalize_mac_address(&raw) else {
                continue;
            };
            if mac_address == "00:00:00:00:00:00" {
                continue;
            }

            mac_addresses.push(mac_address);
        }

        mac_addresses.sort();
        mac_addresses.dedup();
        mac_addresses
    }

    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

pub async fn collect_system_info(
    wireguard_config: Option<&WireGuardConfig>,
) -> Result<SystemInfoPayload> {
    let mut system = System::new_all();
    system.refresh_memory();
    system.refresh_cpu_all();

    let version_metadata = load_version_metadata();
    let cpus = system.cpus();
    let info = SystemInfo {
        os_platform: env::consts::OS.to_owned(),
        os_release: System::kernel_version().unwrap_or_else(|| "unknown".to_owned()),
        os_arch: env::consts::ARCH.to_owned(),
        hostname: System::host_name().unwrap_or_else(|| "unknown".to_owned()),
        cpu_model: cpus
            .first()
            .map(|cpu| cpu.brand().trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_owned()),
        cpu_cores: cpus.len().max(1) as u32,
        mem_total: system.total_memory().max(1),
        agent_version: version_metadata.as_ref().map(|value| value.version.clone()),
        agent_commit: version_metadata
            .as_ref()
            .and_then(|value| value.commit.clone()),
        agent_built_at: version_metadata
            .as_ref()
            .and_then(|value| value.built_at.clone()),
        wireguard: collect_wireguard_status(wireguard_config).await,
        gpus: collect_gpu_info().await,
    };

    let hash = create_info_hash(&info)?;
    Ok(SystemInfoPayload {
        v: 1,
        ts: unix_timestamp_ms(),
        hash,
        info,
    })
}

fn load_version_metadata() -> Option<VersionMetadata> {
    VERSION_METADATA
        .get_or_init(|| {
            for candidate in version_file_candidates() {
                let raw = match fs::read_to_string(&candidate) {
                    Ok(raw) => raw,
                    Err(_) => continue,
                };

                let parsed = match serde_json::from_str::<VersionMetadata>(&raw) {
                    Ok(parsed) if !parsed.version.trim().is_empty() => parsed,
                    _ => continue,
                };

                return Some(VersionMetadata {
                    version: parsed.version.trim().to_owned(),
                    commit: parsed.commit.map(|value| value.trim().to_owned()),
                    built_at: parsed
                        .built_at
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty()),
                });
            }

            None
        })
        .clone()
}

fn version_file_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(value) = env::var("STATIX_VERSION_FILE") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            candidates.push(PathBuf::from(trimmed));
        }
    }

    candidates.push(PathBuf::from("version.json"));

    if let Ok(executable) = env::current_exe() {
        if let Some(parent) = executable.parent() {
            candidates.push(parent.join("version.json"));
        }
    }

    candidates
}

fn normalize_mac_address(value: &str) -> Option<String> {
    let hex: String = value
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    if hex.len() != 12 {
        return None;
    }

    let mut parts = Vec::with_capacity(6);
    for index in (0..12).step_by(2) {
        parts.push(hex[index..index + 2].to_owned());
    }

    Some(parts.join(":"))
}

async fn collect_gpu_info() -> Vec<GpuInfo> {
    if let Some(gpus) = try_collect_nvidia_smi_gpus().await {
        return gpus;
    }

    if let Some(gpus) = try_collect_linux_pci_gpus().await {
        return gpus;
    }

    Vec::new()
}

async fn try_collect_nvidia_smi_gpus() -> Option<Vec<GpuInfo>> {
    let output = run_command_with_timeout(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total,driver_version",
            "--format=csv,noheader,nounits",
        ],
    )
    .await?;

    let gpus: Vec<GpuInfo> = output
        .lines()
        .filter_map(|line| {
            let parts: Vec<_> = line.split(',').map(|part| part.trim()).collect();
            if parts.is_empty() || parts[0].is_empty() {
                return None;
            }

            let memory_bytes = parts
                .get(1)
                .and_then(|value| value.parse::<u64>().ok())
                .map(|value| value.saturating_mul(1024 * 1024));

            Some(GpuInfo {
                name: parts[0].to_owned(),
                vendor: Some("NVIDIA".to_owned()),
                memory_bytes,
                driver_version: parts
                    .get(2)
                    .map(|value| value.to_string())
                    .filter(|value| !value.is_empty()),
            })
        })
        .collect();

    if gpus.is_empty() { None } else { Some(gpus) }
}

async fn try_collect_linux_pci_gpus() -> Option<Vec<GpuInfo>> {
    if !cfg!(target_os = "linux") {
        return None;
    }

    let output = run_command_with_timeout("lspci", &[]).await?;
    let gpus: Vec<GpuInfo> = output
        .lines()
        .filter(|line| {
            let normalized = line.to_ascii_lowercase();
            normalized.contains("vga compatible controller")
                || normalized.contains("3d controller")
                || normalized.contains("display controller")
        })
        .map(|line| {
            let descriptor = line
                .split_once(':')
                .map(|(_, value)| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "GPU".to_owned());
            let normalized = descriptor.to_ascii_lowercase();
            let vendor = if normalized.contains("nvidia") {
                Some("NVIDIA".to_owned())
            } else if normalized.contains("advanced micro devices")
                || normalized.contains("amd")
                || normalized.contains("ati")
            {
                Some("AMD".to_owned())
            } else if normalized.contains("intel") {
                Some("Intel".to_owned())
            } else {
                None
            };

            GpuInfo {
                name: descriptor,
                vendor,
                memory_bytes: None,
                driver_version: None,
            }
        })
        .collect();

    if gpus.is_empty() { None } else { Some(gpus) }
}

async fn run_command_with_timeout(program: &str, args: &[&str]) -> Option<String> {
    let output = timeout(
        std::time::Duration::from_millis(2_500),
        Command::new(program).args(args).output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim().to_owned();
    if trimmed.is_empty() {
        return None;
    }

    Some(trimmed)
}

fn create_info_hash(info: &SystemInfo) -> Result<String> {
    let bytes = serde_json::to_vec(info)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn unix_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::normalize_mac_address;

    #[test]
    fn normalize_mac_address_accepts_colon_separated_values() {
        assert_eq!(
            normalize_mac_address("AA:BB:CC:DD:EE:FF"),
            Some("aa:bb:cc:dd:ee:ff".to_owned())
        );
    }

    #[test]
    fn normalize_mac_address_accepts_compact_values() {
        assert_eq!(
            normalize_mac_address("aabbccddeeff"),
            Some("aa:bb:cc:dd:ee:ff".to_owned())
        );
    }

    #[test]
    fn normalize_mac_address_rejects_invalid_values() {
        assert_eq!(normalize_mac_address("not-a-mac"), None);
    }
}
