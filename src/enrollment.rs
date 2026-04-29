use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::{
    config::{
        LoginConfig, PersistedAgentConfig, WireGuardConfig, WireGuardPeerConfig,
        save_persisted_config,
    },
    system_info::{collect_mac_addresses, collect_system_info},
    wireguard::resolve_or_generate_identity,
};

#[derive(Debug, Clone)]
pub struct LoginOptions {
    pub api_base_url: Option<String>,
    pub requested_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct EnrollmentStartRequest<'a> {
    #[serde(rename = "requestedName", skip_serializing_if = "Option::is_none")]
    requested_name: Option<&'a str>,
    #[serde(rename = "agentMetadata")]
    agent_metadata: AgentMetadata<'a>,
}

#[derive(Debug, Serialize)]
struct AgentMetadata<'a> {
    hostname: &'a str,
    #[serde(rename = "osPlatform")]
    os_platform: &'a str,
    #[serde(rename = "osArch")]
    os_arch: &'a str,
    #[serde(rename = "agentVersion", skip_serializing_if = "Option::is_none")]
    agent_version: Option<&'a str>,
    #[serde(rename = "macAddresses", skip_serializing_if = "Option::is_none")]
    mac_addresses: Option<&'a [String]>,
    #[serde(rename = "wireguardPublicKey", skip_serializing_if = "Option::is_none")]
    wireguard_public_key: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct EnrollmentStartResponse {
    #[serde(rename = "enrollmentToken")]
    enrollment_token: String,
    #[serde(rename = "verificationUrl")]
    verification_url: String,
    #[serde(rename = "userCode")]
    user_code: String,
    #[serde(rename = "expiresAt")]
    expires_at: String,
    #[serde(rename = "pollIntervalMs")]
    poll_interval_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status")]
enum EnrollmentPollResponse {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "approved")]
    Approved {
        #[serde(rename = "nodeId")]
        node_id: String,
        #[serde(rename = "nodeToken")]
        node_token: String,
        #[serde(rename = "agentWsUrl")]
        agent_ws_url: String,
        wireguard: Option<EnrollmentWireGuardConfig>,
    },
}

#[derive(Debug, Deserialize)]
struct EnrollmentWireGuardConfig {
    #[serde(rename = "interfaceName")]
    interface_name: String,
    addresses: Vec<String>,
    #[serde(default)]
    dns: Vec<String>,
    server: EnrollmentWireGuardPeerConfig,
}

#[derive(Debug, Deserialize)]
struct EnrollmentWireGuardPeerConfig {
    #[serde(rename = "publicKey")]
    public_key: String,
    endpoint: String,
    #[serde(rename = "allowedIps")]
    allowed_ips: Vec<String>,
    #[serde(rename = "presharedKey")]
    preshared_key: Option<String>,
    #[serde(rename = "persistentKeepaliveSeconds")]
    persistent_keepalive_seconds: Option<u16>,
}

fn check_dependencies() -> Result<()> {
    let required_cmds = ["wg", "ip", "tar"];
    let mut missing = Vec::new();

    for cmd in required_cmds.iter() {
        if let Err(e) = std::process::Command::new(cmd).arg("--version").output() {
            if e.kind() == std::io::ErrorKind::NotFound {
                missing.push(*cmd);
            }
        }
    }

    if !missing.is_empty() {
        bail!(
            "Missing required dependencies: {}. Please install them before proceeding.",
            missing.join(", ")
        );
    }

    Ok(())
}

pub async fn run_login(login_config: LoginConfig, options: LoginOptions) -> Result<()> {
    check_dependencies()?;

    let client = Client::new();
    let wireguard_identity = resolve_or_generate_identity().await?;
    let system_info = collect_system_info(None).await?;
    let mac_addresses = collect_mac_addresses();
    let requested_name = options
        .requested_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let hostname = system_info.info.hostname.trim();
            if hostname.is_empty() {
                None
            } else {
                Some(hostname)
            }
        });

    let start_response = client
        .post(format!("{}/nodes/enroll/start", login_config.api_base_url))
        .json(&EnrollmentStartRequest {
            requested_name,
            agent_metadata: AgentMetadata {
                hostname: &system_info.info.hostname,
                os_platform: &system_info.info.os_platform,
                os_arch: &system_info.info.os_arch,
                agent_version: system_info.info.agent_version.as_deref(),
                mac_addresses: (!mac_addresses.is_empty()).then_some(mac_addresses.as_slice()),
                wireguard_public_key: Some(&wireguard_identity.public_key),
            },
        })
        .send()
        .await
        .context("failed to start node enrollment")?;

    let status = start_response.status();
    let start_payload = start_response
        .text()
        .await
        .context("failed to read enrollment response")?;
    if !status.is_success() {
        bail!(
            "enrollment start failed ({}): {}",
            status.as_u16(),
            if start_payload.is_empty() {
                "unknown error"
            } else {
                start_payload.as_str()
            }
        );
    }

    let started: EnrollmentStartResponse =
        serde_json::from_str(&start_payload).context("invalid enrollment start payload")?;

    println!("Open this link in your browser and sign in to approve the node:");
    println!("{}", started.verification_url);
    println!("Code: {}", started.user_code);
    println!("Expires: {}", started.expires_at);
    println!("Waiting for approval...");

    loop {
        let poll_response = client
            .post(format!("{}/nodes/enroll/poll", login_config.api_base_url))
            .json(&serde_json::json!({
                "token": started.enrollment_token,
            }))
            .send()
            .await
            .context("failed to poll enrollment")?;

        let poll_status = poll_response.status();
        let poll_payload = poll_response
            .text()
            .await
            .context("failed to read poll response")?;

        if poll_status == reqwest::StatusCode::CONFLICT {
            bail!(
                "enrollment conflict: {}",
                if poll_payload.is_empty() {
                    "unknown error"
                } else {
                    poll_payload.as_str()
                }
            );
        }
        if poll_status == reqwest::StatusCode::GONE {
            bail!("enrollment expired");
        }
        if !poll_status.is_success() {
            bail!(
                "enrollment poll failed ({}): {}",
                poll_status.as_u16(),
                if poll_payload.is_empty() {
                    "unknown error"
                } else {
                    poll_payload.as_str()
                }
            );
        }

        let poll: EnrollmentPollResponse =
            serde_json::from_str(&poll_payload).context("invalid enrollment poll payload")?;
        match poll {
            EnrollmentPollResponse::Pending => {
                sleep(Duration::from_millis(started.poll_interval_ms.max(1_000))).await;
            }
            EnrollmentPollResponse::Approved {
                node_id,
                node_token,
                agent_ws_url,
                wireguard,
            } => {
                let wireguard = wireguard.map(|value| WireGuardConfig {
                    interface_name: value.interface_name,
                    addresses: value.addresses,
                    dns: value.dns,
                    private_key: wireguard_identity.private_key.clone(),
                    public_key: wireguard_identity.public_key.clone(),
                    server: WireGuardPeerConfig {
                        public_key: value.server.public_key,
                        endpoint: value.server.endpoint,
                        allowed_ips: value.server.allowed_ips,
                        preshared_key: value.server.preshared_key,
                        persistent_keepalive_seconds: value.server.persistent_keepalive_seconds,
                    },
                });
                let path = save_persisted_config(&PersistedAgentConfig {
                    node_id: node_id.clone(),
                    node_token: node_token.clone(),
                    agent_ws_url: agent_ws_url.clone(),
                    api_base_url: Some(login_config.api_base_url.clone()),
                    wireguard: wireguard.clone(),
                })?;

                let finish_response = client
                    .post(format!("{}/nodes/enroll/finish", login_config.api_base_url))
                    .json(&serde_json::json!({
                        "token": started.enrollment_token,
                    }))
                    .send()
                    .await
                    .context("failed to finalize enrollment")?;

                if !finish_response.status().is_success() {
                    let text = finish_response
                        .text()
                        .await
                        .unwrap_or_else(|_| "unable to read finalize response".to_owned());
                    eprintln!(
                        "[statix-agent] warning: enrollment finished locally but finalize failed: {}",
                        text
                    );
                }

                println!("Node enrolled successfully.");
                println!("Node ID: {}", node_id);
                println!("Agent websocket: {}", agent_ws_url);
                if let Some(wireguard) = wireguard.as_ref() {
                    println!(
                        "WireGuard: {} {}",
                        wireguard.interface_name,
                        wireguard.addresses.join(", ")
                    );
                }
                println!("Saved config: {}", path.display());
                return Ok(());
            }
        }
    }
}
