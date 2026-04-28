use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::collections::VecDeque;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

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

struct QemuProcess {
    child: Option<Child>,
    recent_logs: Arc<Mutex<VecDeque<String>>>,
}

impl QemuProcess {
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };

        let status = child.try_wait().context("failed to poll qemu status")?;
        if status.is_some() {
            self.child.take();
        }

        Ok(status)
    }

    fn failure_message(&self, status: std::process::ExitStatus) -> String {
        let logs = self
            .recent_logs
            .lock()
            .ok()
            .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();

        if logs.trim().is_empty() {
            format!("qemu exited before the guest became ready: {status}")
        } else {
            format!("qemu exited before the guest became ready: {status}\nrecent qemu logs:\n{logs}")
        }
    }

    async fn shutdown(&mut self) {
        if let Ok(Some(status)) = self.try_wait() {
            log_qemu_exit_status(Ok(status));
            return;
        }

        if let Some(mut child) = self.child.take() {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = child.kill();
                log_qemu_exit_status(child.wait());
            })
            .await;
        }
    }
}

impl Drop for QemuProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            log_qemu_exit_status(child.wait());
        }
    }
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
        eprintln!(
            "[statix-agent] job {}: preparing microvm runtime at {}",
            ctx.job_id,
            runtime_root.display()
        );

        let base_image = resolve_base_image(&self.image, &runtime_root).await?;
        eprintln!(
            "[statix-agent] job {}: using microvm base image {}",
            ctx.job_id,
            base_image.display()
        );
        let overlay_image = runtime_root.join("disk.qcow2");
        create_overlay_disk(&base_image, &overlay_image).await?;
        eprintln!(
            "[statix-agent] job {}: created microvm overlay disk {}",
            ctx.job_id,
            overlay_image.display()
        );

        let ssh_dir = runtime_root.join("ssh");
        fs::create_dir_all(&ssh_dir).with_context(|| format!("failed to create {}", ssh_dir.display()))?;
        let private_key = ssh_dir.join("id_ed25519");
        let public_key = ssh_dir.join("id_ed25519.pub");
        generate_ssh_keypair(&private_key, &public_key).await?;
        eprintln!("[statix-agent] job {}: generated microvm ssh keypair", ctx.job_id);

        let seed_iso = runtime_root.join("seed.iso");
        let workspace_tar = runtime_root.join("workspace.tar.gz");
        create_workspace_archive(&workspace_tar, &workspace.workdir).await?;
        eprintln!(
            "[statix-agent] job {}: archived workspace {}",
            ctx.job_id,
            workspace.workdir.display()
        );
        create_cloud_init_seed(&seed_iso, &public_key).await?;
        eprintln!(
            "[statix-agent] job {}: created microvm cloud-init seed {}",
            ctx.job_id,
            seed_iso.display()
        );

        let mut qemu = launch_qemu(
            &overlay_image,
            &seed_iso,
            self.cpu.unwrap_or(DEFAULT_CPU_COUNT),
            self.memory_mb.unwrap_or(DEFAULT_MEMORY_MB),
            DEFAULT_SSH_PORT,
        )
        .await?;
        eprintln!(
            "[statix-agent] job {}: launched microvm with {} cpu(s), {} MiB memory, ssh port {}",
            ctx.job_id,
            self.cpu.unwrap_or(DEFAULT_CPU_COUNT),
            self.memory_mb.unwrap_or(DEFAULT_MEMORY_MB),
            DEFAULT_SSH_PORT
        );

        let result = match wait_for_guest_ready(
            &mut qemu,
            DEFAULT_SSH_PORT,
            &private_key,
            ctx.timeout_seconds,
        )
        .await
        {
            Ok(()) => {
                eprintln!("[statix-agent] job {}: microvm guest is ready", ctx.job_id);
                run_guest_command(
                    DEFAULT_SSH_PORT,
                    &private_key,
                    ctx.timeout_seconds,
                    command,
                    workspace,
                    &workspace_tar,
                )
                .await
            }
            Err(error) => {
                eprintln!(
                    "[statix-agent] job {}: microvm readiness failed: {error:#}",
                    ctx.job_id
                );
                Err(error)
            }
        };

        eprintln!("[statix-agent] job {}: shutting down microvm", ctx.job_id);
        qemu.shutdown().await;

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
        .with_context(|| missing_dependency_message("qemu-img", "qemu-utils"))?;

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
        .with_context(|| missing_dependency_message("cloud-localds", "cloud-image-utils"))?;

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
) -> Result<QemuProcess> {
    let binary = qemu_binary()?;
    let recent_logs = Arc::new(Mutex::new(VecDeque::with_capacity(32)));
    let mut command = Command::new(binary);
    command.args(qemu_launch_args(overlay_image, seed_iso, cpu, memory_mb, ssh_port));

    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| missing_dependency_message(binary, qemu_package_hint()))?;
    spawn_qemu_log_stream("stdout", child.stdout.take(), Arc::clone(&recent_logs));
    spawn_qemu_log_stream("stderr", child.stderr.take(), Arc::clone(&recent_logs));
    Ok(QemuProcess { child: Some(child), recent_logs })
}

fn qemu_launch_args(
    overlay_image: &Path,
    seed_iso: &Path,
    cpu: u8,
    memory_mb: u32,
    ssh_port: u16,
) -> Vec<String> {
    vec![
        "-m".to_string(),
        memory_mb.to_string(),
        "-smp".to_string(),
        cpu.to_string(),
        "-accel".to_string(),
        "kvm".to_string(),
        "-accel".to_string(),
        "tcg,split-wx=on,thread=multi".to_string(),
        "-drive".to_string(),
        format!("if=virtio,format=qcow2,file={}", overlay_image.display()),
        "-drive".to_string(),
        format!("if=virtio,format=raw,file={},media=cdrom", seed_iso.display()),
        "-netdev".to_string(),
        format!("user,id=net0,hostfwd=tcp::{}-:22", ssh_port),
        "-device".to_string(),
        "virtio-net-pci,netdev=net0".to_string(),
        "-nographic".to_string(),
    ]
}

fn spawn_qemu_log_stream(
    label: &'static str,
    stream: Option<impl std::io::Read + Send + 'static>,
    recent_logs: Arc<Mutex<VecDeque<String>>>,
) {
    let Some(stream) = stream else {
        return;
    };

    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if let Ok(mut logs) = recent_logs.lock() {
                        if logs.len() == 32 {
                            logs.pop_front();
                        }
                        logs.push_back(format!("{label}: {line}"));
                    }

                    if verbose_logs_enabled() {
                        eprintln!("[statix-agent][debug] qemu {label}: {line}");
                    }
                }
                Err(error) => {
                    eprintln!("[statix-agent] qemu {label} log read failed: {error}");
                    break;
                }
            }
        }
    });
}

fn log_qemu_exit_status(result: std::io::Result<std::process::ExitStatus>) {
    match result {
        Ok(status) if status.success() => {
            if verbose_logs_enabled() {
                eprintln!("[statix-agent][debug] qemu exited with status: {status}");
            }
        }
        Ok(status) => eprintln!("[statix-agent] qemu exited with status: {status}"),
        Err(error) => eprintln!("[statix-agent] failed to wait for qemu: {error}"),
    }
}

fn verbose_logs_enabled() -> bool {
    matches!(
        std::env::var("STATIX_VERBOSE_LOGS")
            .ok()
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

async fn wait_for_guest_ready(
    qemu: &mut QemuProcess,
    ssh_port: u16,
    private_key: &Path,
    timeout_seconds: u64,
) -> Result<()> {
    let deadline = Duration::from_secs(timeout_seconds);
    let start = Instant::now();
    let mut next_progress_log = Duration::from_secs(15);

    loop {
        if let Some(status) = qemu.try_wait()? {
            bail!(qemu.failure_message(status));
        }

        if start.elapsed() >= deadline {
            bail!("microvm did not become ready before timeout");
        }

        let remaining = deadline.saturating_sub(start.elapsed());
        let mut probe = TokioCommand::new("ssh");
        probe
            .arg("-i")
            .arg(private_key)
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

        let last_probe_failure =
            match timeout(remaining.min(Duration::from_secs(5)), probe.output()).await {
            Ok(Ok(output)) if output.status.success() => return Ok(()),
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                if stderr.is_empty() {
                    format!("ssh readiness probe exited with {}", output.status)
                } else {
                    format!(
                        "ssh readiness probe exited with {}: {}",
                        output.status,
                        truncate_for_log(&stderr, 240)
                    )
                }
            }
            Ok(Err(error)) => format!("failed to launch ssh readiness probe: {error}"),
            Err(_) => "ssh readiness probe timed out".to_string(),
            };

        let elapsed = start.elapsed();
        if elapsed >= next_progress_log {
            eprintln!(
                "[statix-agent] waiting for microvm guest readiness for {}s; last probe: {}",
                elapsed.as_secs(),
                last_probe_failure
            );
            next_progress_log += Duration::from_secs(15);
        }

        sleep(Duration::from_secs(2)).await;
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
    eprintln!(
        "[statix-agent] uploading workspace archive to microvm from {}",
        workspace_tar.display()
    );
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
                .arg(format!(
                    "{}@127.0.0.1:/home/{}/workspace.tar.gz",
                    DEFAULT_SSH_USER, DEFAULT_SSH_USER
                ));
            scp.output().await
        },
    )
    .await
    .map_err(|_| anyhow!("microvm archive upload timed out after {} seconds", timeout_seconds))?
    .map_err(anyhow::Error::from)
    .map_err(|error| error.context("failed to upload workspace archive to microvm"))?;

    if !upload.status.success() {
        eprintln!(
            "[statix-agent] microvm workspace archive upload failed with {}",
            upload.status
        );
        return Ok(JobExecutionResult {
            status: "failed",
            message: Some(summarize_command_output(&workspace.workdir, &upload.stdout, &upload.stderr)),
        });
    }
    eprintln!("[statix-agent] uploaded workspace archive to microvm");

    let remote_command = format!(
        "mkdir -p /home/{user}/workspace && tar -xzf /home/{user}/workspace.tar.gz -C /home/{user}/workspace && cd /home/{user}/workspace && exec {command}",
        user = DEFAULT_SSH_USER,
        command = shell_join(command)
    );

    eprintln!(
        "[statix-agent] running command inside microvm: {}",
        shell_join(command)
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
        eprintln!("[statix-agent] microvm command succeeded");
        Ok(JobExecutionResult { status: "succeeded", message: Some(message) })
    } else {
        eprintln!("[statix-agent] microvm command failed with {}", output.status);
        Ok(JobExecutionResult { status: "failed", message: Some(message) })
    }
}

fn truncate_for_log(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let mut shortened = value.chars().take(max_chars).collect::<String>();
    shortened.push_str("...");
    shortened
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

fn qemu_package_hint() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "qemu-system-x86",
        "aarch64" => "qemu-system-arm",
        _ => "qemu-system",
    }
}

fn missing_dependency_message(program: &str, debian_package: &str) -> String {
    format!(
        "failed to launch {program}; install the '{debian_package}' package and ensure {program} is on PATH"
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_launch_args_enable_kvm_fallback_and_split_wx_tcg() {
        let overlay_image = Path::new("/tmp/disk.qcow2");
        let seed_iso = Path::new("/tmp/seed.iso");

        let args = qemu_launch_args(overlay_image, seed_iso, 4, 2048, 2222);

        assert_eq!(
            args,
            vec![
                "-m",
                "2048",
                "-smp",
                "4",
                "-accel",
                "kvm",
                "-accel",
                "tcg,split-wx=on,thread=multi",
                "-drive",
                "if=virtio,format=qcow2,file=/tmp/disk.qcow2",
                "-drive",
                "if=virtio,format=raw,file=/tmp/seed.iso,media=cdrom",
                "-netdev",
                "user,id=net0,hostfwd=tcp::2222-:22",
                "-device",
                "virtio-net-pci,netdev=net0",
                "-nographic",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }
}
