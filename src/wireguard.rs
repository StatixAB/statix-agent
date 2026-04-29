use std::{fs, path::PathBuf, process::Stdio};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

use crate::config::{WireGuardConfig, agent_state_dir, load_persisted_config_snapshot};

const WG_COMMAND_TIMEOUT_MS: u64 = 8_000;

#[derive(Debug, Clone)]
pub struct WireGuardIdentity {
    pub private_key: String,
    pub public_key: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WireGuardStatus {
    #[serde(rename = "interfaceName")]
    pub interface_name: String,
    #[serde(rename = "publicKey")]
    pub public_key: String,
    pub addresses: Vec<String>,
    pub connected: bool,
    #[serde(rename = "lastHandshakeAt", skip_serializing_if = "Option::is_none")]
    pub last_handshake_at: Option<u64>,
    #[serde(rename = "peerEndpoint", skip_serializing_if = "Option::is_none")]
    pub peer_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn resolve_or_generate_identity() -> Result<WireGuardIdentity> {
    if let Some(existing) = load_persisted_config_snapshot()?
        .and_then(|value| value.wireguard)
        .filter(|value| !value.private_key.trim().is_empty() && !value.public_key.trim().is_empty())
    {
        return Ok(WireGuardIdentity {
            private_key: existing.private_key,
            public_key: existing.public_key,
        });
    }

    generate_identity().await
}

pub async fn ensure_applied(config: &WireGuardConfig) -> Result<PathBuf> {
    if !cfg!(target_os = "linux") {
        bail!("wireguard auto-apply is currently supported on Linux only");
    }

    let path = write_runtime_config(config)?;
    let _ = run_command("wg-quick", &["down", path.to_string_lossy().as_ref()]).await;
    run_command("wg-quick", &["up", path.to_string_lossy().as_ref()])
        .await
        .context("failed to bring up wireguard interface")?;
    Ok(path)
}

pub async fn collect_status(config: Option<&WireGuardConfig>) -> Option<WireGuardStatus> {
    let config = config?;

    let wg_output = run_command("wg", &["show", config.interface_name.as_str(), "dump"]).await;
    let addresses = match run_command(
        "ip",
        &[
            "-o",
            "address",
            "show",
            "dev",
            config.interface_name.as_str(),
        ],
    )
    .await
    {
        Ok(output) => parse_interface_addresses(&output, &config.addresses),
        Err(_) => config.addresses.clone(),
    };

    match wg_output {
        Ok(output) => Some(parse_status_dump(config, &output, addresses)),
        Err(error) => Some(WireGuardStatus {
            interface_name: config.interface_name.clone(),
            public_key: config.public_key.clone(),
            addresses,
            connected: false,
            last_handshake_at: None,
            peer_endpoint: None,
            error: Some(error.to_string()),
        }),
    }
}

async fn generate_identity() -> Result<WireGuardIdentity> {
    let private_key = run_command("wg", &["genkey"])
        .await
        .context("failed to generate wireguard private key")?;
    let public_key = run_command_with_stdin("wg", &["pubkey"], private_key.as_bytes())
        .await
        .context("failed to derive wireguard public key")?;

    Ok(WireGuardIdentity {
        private_key,
        public_key,
    })
}

fn write_runtime_config(config: &WireGuardConfig) -> Result<PathBuf> {
    let directory = agent_state_dir()?.join("wireguard");
    fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;

    let path = directory.join(format!("{}.conf", config.interface_name));
    fs::write(&path, render_config(config))
        .with_context(|| format!("failed to write {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let permissions = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&path, permissions)
            .with_context(|| format!("failed to protect {}", path.display()))?;
    }

    Ok(path)
}

fn render_config(config: &WireGuardConfig) -> String {
    let mut lines = vec![
        "[Interface]".to_owned(),
        format!("PrivateKey = {}", config.private_key),
        format!("Address = {}", config.addresses.join(", ")),
    ];

    if !config.dns.is_empty() {
        lines.push(format!("DNS = {}", config.dns.join(", ")));
    }

    lines.push(String::new());
    lines.push("[Peer]".to_owned());
    lines.push(format!("PublicKey = {}", config.server.public_key));
    lines.push(format!("Endpoint = {}", config.server.endpoint));
    lines.push(format!(
        "AllowedIPs = {}",
        config.server.allowed_ips.join(", ")
    ));

    if let Some(preshared_key) = config.server.preshared_key.as_ref() {
        lines.push(format!("PresharedKey = {preshared_key}"));
    }

    if let Some(keepalive) = config.server.persistent_keepalive_seconds {
        lines.push(format!("PersistentKeepalive = {keepalive}"));
    }

    lines.push(String::new());
    lines.join("\n")
}

fn parse_status_dump(
    config: &WireGuardConfig,
    output: &str,
    addresses: Vec<String>,
) -> WireGuardStatus {
    let mut lines = output.lines();
    let interface_line = lines.next().unwrap_or_default();
    let interface_parts: Vec<_> = interface_line.split('\t').collect();
    let public_key = interface_parts
        .get(1)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or(config.public_key.as_str())
        .to_owned();

    let mut last_handshake_at = None;
    let mut peer_endpoint = None;
    for line in lines {
        let parts: Vec<_> = line.split('\t').collect();
        if parts.is_empty() {
            continue;
        }

        if peer_endpoint.is_none() {
            peer_endpoint = parts
                .get(2)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty() && value != "(none)");
        }

        if last_handshake_at.is_none() {
            last_handshake_at = parts
                .get(4)
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|value| *value > 0);
        }
    }

    WireGuardStatus {
        interface_name: config.interface_name.clone(),
        public_key,
        addresses,
        connected: last_handshake_at.is_some(),
        last_handshake_at,
        peer_endpoint,
        error: None,
    }
}

fn parse_interface_addresses(output: &str, fallback: &[String]) -> Vec<String> {
    let addresses: Vec<String> = output
        .lines()
        .filter_map(|line| {
            let parts: Vec<_> = line.split_whitespace().collect();
            match parts.get(2).copied() {
                Some("inet") | Some("inet6") => parts.get(3).map(|value| value.to_string()),
                _ => None,
            }
        })
        .collect();

    if addresses.is_empty() {
        fallback.to_vec()
    } else {
        addresses
    }
}

async fn run_command(program: &str, args: &[&str]) -> Result<String> {
    let output = timeout(
        std::time::Duration::from_millis(WG_COMMAND_TIMEOUT_MS),
        Command::new(program).args(args).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{program} timed out"))?
    .with_context(|| format!("failed to spawn {program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if stderr.is_empty() {
            bail!("{program} exited with status {}", output.status);
        }
        bail!("{program} failed: {stderr}");
    }

    let stdout = String::from_utf8(output.stdout).context("command output was not utf-8")?;
    let trimmed = stdout.trim().to_owned();
    if trimmed.is_empty() {
        bail!("{program} returned empty output");
    }

    Ok(trimmed)
}

async fn run_command_with_stdin(program: &str, args: &[&str], input: &[u8]) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;

    let mut stdin = child
        .stdin
        .take()
        .context("child process stdin was not available")?;
    stdin.write_all(input).await?;
    stdin.write_all(b"\n").await?;
    drop(stdin);

    let output = timeout(
        std::time::Duration::from_millis(WG_COMMAND_TIMEOUT_MS),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{program} timed out"))??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if stderr.is_empty() {
            bail!("{program} exited with status {}", output.status);
        }
        bail!("{program} failed: {stderr}");
    }

    let stdout = String::from_utf8(output.stdout).context("command output was not utf-8")?;
    let trimmed = stdout.trim().to_owned();
    if trimmed.is_empty() {
        bail!("{program} returned empty output");
    }

    Ok(trimmed)
}
