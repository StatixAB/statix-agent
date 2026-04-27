use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use tokio::{process::Command as TokioCommand, time::{sleep, timeout, Duration}};

use crate::config::agent_state_dir;
use crate::jobs::{summarize_command_output, ExecutionContext, JobExecutionResult, PreparedWorkspace, Runner};

const DEFAULT_SSH_USER: &str = "statix";
const DEFAULT_SSH_PORT: u16 = 2222;
const DEFAULT_MEMORY_MB: u32 = 4096;
const DEFAULT_CPU_COUNT: u8 = 2;

pub struct MicrovmRunner {
    image: String,
    cpu: Option<u8>,
    memory_mb: Option<u32>,
}

impl MicrovmRunner {
    pub fn new(image: String, cpu: Option<u8>, memory_mb: Option<u32>) -> Self {
        Self { image, cpu, memory_mb }
    }
}

#[async_trait::async_trait]
impl Runner for MicrovmRunner {
    async fn execute(
        &self,
        ctx: &ExecutionContext,
        workspace: &PreparedWorkspace,
        command: &[String],
    ) -> Result<JobExecutionResult> {
        if ctx.timeout_seconds == 0 || ctx.timeout_seconds > 3600 {
            bail!("run timeoutSeconds must be between 1 and 3600");
        }
        if command.is_empty() {
            bail!("run command must contain at least one token");
        }

        let runtime_root = agent_state_dir()?.join("microvm").join(&ctx.job_id).join(&ctx.attempt_id);
        fs::create_dir_all(&runtime_root).with_context(|| format!("failed to create {}", runtime_root.display()))?;

        let base_image = resolve_base_image(&self.image, &runtime_root).await?;
        let overlay_image = runtime_root.join("disk.qcow2");
        create_overlay_disk(&base_image, &overlay_image).await?;

        let ssh_dir = runtime_root.join("ssh");
        fs::create_dir_all(&ssh_dir).with_context(|| format!("failed to create {}", ssh_dir.display()))?;
        let private_key = ssh_dir.join("id_ed25519");
        let public_key = ssh_dir.join("id_ed25519.pub");
        generate_ssh_keypair(&private_key, &public_key).await?;

        let seed_iso = runtime_root.join("seed.iso");
        let workspace_tar = runtime_root.join("workspace.tar.gz");
        create_workspace_archive(&workspace_tar, &workspace.workdir).await?;
        create_cloud_init_seed(&seed_iso, &public_key).await?;

        let mut qemu = launch_qemu(
            &overlay_image,
            &seed_iso,
            self.cpu.unwrap_or(DEFAULT_CPU_COUNT),
            self.memory_mb.unwrap_or(DEFAULT_MEMORY_MB),
            DEFAULT_SSH_PORT,
        )
        .await?;

        let result = match wait_for_guest_ready(DEFAULT_SSH_PORT, ctx.timeout_seconds).await {
            Ok(()) => run_guest_command(
                DEFAULT_SSH_PORT,
                &private_key,
                ctx.timeout_seconds,
                command,
                workspace,
                &workspace_tar,
            )
            .await,
            Err(error) => Err(error),
        };

        let _ = qemu.kill().await;
        let _ = qemu.wait().await;

        result
    }
}

async fn resolve_base_image(image: &str, runtime_root: &Path) -> Result<PathBuf> {
    let trimmed = image.trim();
    if trimmed.is_empty() {
        bail!("microvm image must not be empty");
    }

    let path = Path::new(trimmed);
    if path.exists() {
        return Ok(path.to_path_buf());
    }

    let url = match trimmed {
        "ubuntu-24.04" | "ubuntu-24.04-lts" | "ubuntu-noble" => canonical_ubuntu_image_url(),
        value if value.starts_with("http://") || value.starts_with("https://") => value.to_owned(),
        value => bail!("unsupported microvm image reference: {value}"),
    };

    let cache_dir = runtime_root.join("images");
    fs::create_dir_all(&cache_dir).with_context(|| format!("failed to create {}", cache_dir.display()))?;
    let cache_name = format!("{}.img", image_cache_key(&url));
    let cached_image = cache_dir.join(cache_name);
    if cached_image.exists() {
        return Ok(cached_image);
    }

    let bytes = reqwest::get(&url)
        .await
        .with_context(|| format!("failed to download microvm image from {url}"))?
        .error_for_status()
        .with_context(|| format!("microvm image request failed for {url}"))?
        .bytes()
        .await
        .with_context(|| format!("failed to read microvm image from {url}"))?;

    fs::write(&cached_image, &bytes)
        .with_context(|| format!("failed to cache microvm image at {}", cached_image.display()))?;
    Ok(cached_image)
}

async fn create_overlay_disk(base_image: &Path, overlay_image: &Path) -> Result<()> {
    let status = TokioCommand::new("qemu-img")
        .arg("create")
        .arg("-f")
        .arg("qcow2")
        .arg("-F")
        .arg("qcow2")
        .arg("-b")
        .arg(base_image)
        .arg(overlay_image)
        .status()
        .await
        .context("failed to launch qemu-img")?;

    if !status.success() {
        bail!("qemu-img failed to create overlay disk");
    }

    Ok(())
}

async fn generate_ssh_keypair(private_key: &Path, public_key: &Path) -> Result<()> {
    if private_key.exists() {
        let _ = fs::remove_file(private_key);
    }
    if public_key.exists() {
        let _ = fs::remove_file(public_key);
    }

    let status = TokioCommand::new("ssh-keygen")
        .arg("-q")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-f")
        .arg(private_key)
        .status()
        .await
        .context("failed to launch ssh-keygen")?;

    if !status.success() {
        bail!("ssh-keygen failed to create guest keypair");
    }

    if !public_key.exists() {
        bail!("ssh-keygen did not create a public key");
    }

    Ok(())
}

async fn create_workspace_archive(archive_path: &Path, workdir: &Path) -> Result<()> {
    if archive_path.exists() {
        let _ = fs::remove_file(archive_path);
    }

    let status = TokioCommand::new("tar")
        .arg("-C")
        .arg(workdir)
        .arg("-czf")
        .arg(archive_path)
        .arg(".")
        .status()
        .await
        .context("failed to launch tar")?;

    if !status.success() {
        bail!("failed to archive workspace for microvm execution");
    }

    Ok(())
}

async fn create_cloud_init_seed(seed_iso: &Path, public_key: &Path) -> Result<()> {
    let public_key = fs::read_to_string(public_key)
        .with_context(|| format!("failed to read {}", public_key.display()))?;

    let user_data = format!(
        r#"#cloud-config
users:
  - name: {user}
    groups: [sudo]
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - {public_key}
ssh_pwauth: false
disable_root: true
package_update: true
packages:
  - build-essential
  - ca-certificates
  - cargo
  - curl
  - git
  - libssl-dev
  - openssh-server
  - pkg-config
"#,
        user = DEFAULT_SSH_USER,
        public_key = public_key.trim(),
    );

    let meta_data = "instance-id: statix-microvm\nlocal-hostname: statix-microvm\n";
    let user_data_path = seed_iso.with_extension("user-data");
    let meta_data_path = seed_iso.with_extension("meta-data");
    fs::write(&user_data_path, user_data)
        .with_context(|| format!("failed to write {}", user_data_path.display()))?;
    fs::write(&meta_data_path, meta_data)
        .with_context(|| format!("failed to write {}", meta_data_path.display()))?;

    let status = TokioCommand::new("cloud-localds")
        .arg(seed_iso)
        .arg(&user_data_path)
        .arg(&meta_data_path)
        .status()
        .await
        .context("failed to launch cloud-localds")?;

    let _ = fs::remove_file(user_data_path);
    let _ = fs::remove_file(meta_data_path);

    if !status.success() {
        bail!("cloud-localds failed to create the seed image");
    }

    Ok(())
}

async fn launch_qemu(
    overlay_image: &Path,
    seed_iso: &Path,
    cpu: u8,
    memory_mb: u32,
    ssh_port: u16,
) -> Result<tokio::process::Child> {
    let binary = qemu_binary()?;
    let mut command = TokioCommand::new(binary);
    command
        .arg("-m")
        .arg(memory_mb.to_string())
        .arg("-smp")
        .arg(cpu.to_string())
        .arg("-drive")
        .arg(format!("if=virtio,format=qcow2,file={}", overlay_image.display()))
        .arg("-drive")
        .arg(format!("if=virtio,format=raw,file={},media=cdrom", seed_iso.display()))
        .arg("-netdev")
        .arg(format!("user,id=net0,hostfwd=tcp::{}-:22", ssh_port))
        .arg("-device")
        .arg("virtio-net-pci,netdev=net0")
        .arg("-nographic")
        .kill_on_drop(true);

    command.stdout(Stdio::null());
    command.stderr(Stdio::null());

    command.spawn().context("failed to launch qemu")
}

async fn wait_for_guest_ready(ssh_port: u16, timeout_seconds: u64) -> Result<()> {
    let deadline = Duration::from_secs(timeout_seconds);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() >= deadline {
            bail!("microvm did not become ready before timeout");
        }

        let remaining = deadline.saturating_sub(start.elapsed());
        let mut probe = TokioCommand::new("ssh");
        probe
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=3")
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg("-o")
            .arg("UserKnownHostsFile=/dev/null")
            .arg("-p")
            .arg(ssh_port.to_string())
            .arg(format!("{}@127.0.0.1", DEFAULT_SSH_USER))
            .arg("test")
            .arg("-f")
            .arg("/var/lib/cloud/instance/boot-finished");

        match timeout(remaining.min(Duration::from_secs(5)), probe.output()).await {
            Ok(Ok(output)) if output.status.success() => return Ok(()),
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => sleep(Duration::from_secs(2)).await,
        }
    }
}

async fn run_guest_command(
    ssh_port: u16,
    private_key: &Path,
    timeout_seconds: u64,
    command: &[String],
    workspace: &PreparedWorkspace,
    workspace_tar: &Path,
) -> Result<JobExecutionResult> {
    let upload = timeout(
        Duration::from_secs(timeout_seconds),
        async {
            let mut scp = TokioCommand::new("scp");
            scp.arg("-i")
                .arg(private_key)
                .arg("-o")
                .arg("BatchMode=yes")
                .arg("-o")
                .arg("ConnectTimeout=5")
                .arg("-o")
                .arg("StrictHostKeyChecking=no")
                .arg("-o")
                .arg("UserKnownHostsFile=/dev/null")
                .arg("-P")
                .arg(ssh_port.to_string())
                .arg(workspace_tar)
                .arg(format!("{}@127.0.0.1:/home/{}/workspace.tar.gz", DEFAULT_SSH_USER, DEFAULT_SSH_USER));
            scp.output().await
        },
    )
    .await
    .map_err(|_| anyhow!("microvm archive upload timed out after {} seconds", timeout_seconds))?
    .map_err(anyhow::Error::from)
    .map_err(|error| error.context("failed to upload workspace archive to microvm"))?;

    if !upload.status.success() {
        return Ok(JobExecutionResult {
            status: "failed",
            message: Some(summarize_command_output(&workspace.workdir, &upload.stdout, &upload.stderr)),
        });
    }

    let remote_command = format!(
        "mkdir -p /home/{user}/workspace && tar -xzf /home/{user}/workspace.tar.gz -C /home/{user}/workspace && cd /home/{user}/workspace && exec {command}",
        user = DEFAULT_SSH_USER,
        command = shell_join(command)
    );

    let output = timeout(
        Duration::from_secs(timeout_seconds),
        async {
            let mut ssh = TokioCommand::new("ssh");
            ssh.arg("-i")
                .arg(private_key)
                .arg("-o")
                .arg("BatchMode=yes")
                .arg("-o")
                .arg("ConnectTimeout=5")
                .arg("-o")
                .arg("StrictHostKeyChecking=no")
                .arg("-o")
                .arg("UserKnownHostsFile=/dev/null")
                .arg("-p")
                .arg(ssh_port.to_string())
                .arg(format!("{}@127.0.0.1", DEFAULT_SSH_USER))
                .arg("sh")
                .arg("-lc")
                .arg(remote_command)
                .current_dir(&workspace.workdir);
            ssh.output().await
        },
    )
    .await
    .map_err(|_| anyhow!("microvm command timed out after {} seconds", timeout_seconds))?
    .map_err(anyhow::Error::from)
    .map_err(|error| error.context("failed to execute command inside microvm"))?;

    let message = summarize_command_output(&workspace.workdir, &output.stdout, &output.stderr);
    if output.status.success() {
        Ok(JobExecutionResult { status: "succeeded", message: Some(message) })
    } else {
        Ok(JobExecutionResult { status: "failed", message: Some(message) })
    }
}

fn shell_join(command: &[String]) -> String {
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

    if value.chars().all(|character| character.is_ascii_alphanumeric() || "@%_-+=:,./".contains(character)) {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

fn qemu_binary() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("qemu-system-x86_64"),
        "aarch64" => Ok("qemu-system-aarch64"),
        arch => bail!("unsupported host architecture for microvm runner: {arch}"),
    }
}

fn canonical_ubuntu_image_url() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img".to_string(),
        "aarch64" => "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img".to_string(),
        _ => "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img".to_string(),
    }
}

fn image_cache_key(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hex::encode(hasher.finalize())
}