use std::path::Path;

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

pub(super) const DEFAULT_SSH_USER: &str = "statix";
pub(super) const DEFAULT_SSH_PORT: u16 = 2222;
pub(super) const DEFAULT_MEMORY_MB: u32 = 4096;
pub(super) const DEFAULT_CPU_COUNT: u8 = 2;

pub(super) fn truncate_for_log(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let mut shortened = value.chars().take(max_chars).collect::<String>();
    shortened.push_str("...");
    shortened
}

pub(super) fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|value| shell_escape(value))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "@%_-+=:,./".contains(character))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn qemu_binary() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("qemu-system-x86_64"),
        "aarch64" => Ok("qemu-system-aarch64"),
        arch => bail!("unsupported host architecture for microvm runner: {arch}"),
    }
}

pub(super) fn qemu_package_hint() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "qemu-system-x86",
        "aarch64" => "qemu-system-arm",
        _ => "qemu-system",
    }
}

pub(super) fn missing_dependency_message(program: &str, debian_package: &str) -> String {
    format!(
        "failed to launch {program}; install the '{debian_package}' package and ensure {program} is on PATH"
    )
}

pub(super) fn canonical_ubuntu_image_url() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"
            .to_string(),
        "aarch64" => {
            "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img"
                .to_string()
        }
        _ => "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"
            .to_string(),
    }
}

pub(super) fn image_cache_key(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hex::encode(hasher.finalize())
}

pub(super) fn project_vm_key(project_id: &str, environment: &str) -> String {
    format!(
        "{}-{}-{}",
        safe_path_segment(project_id),
        safe_path_segment(environment),
        image_cache_key(&format!("{project_id}:{environment}"))[..12].to_string()
    )
}

pub(super) fn project_vm_ssh_port(vm_key: &str) -> u16 {
    let mut hasher = Sha256::new();
    hasher.update(vm_key.as_bytes());
    let digest = hasher.finalize();
    let value = u16::from_be_bytes([digest[0], digest[1]]);
    22_200 + (value % 1_000)
}

pub(super) fn safe_path_segment(value: &str) -> String {
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

pub(super) fn process_is_running(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}
