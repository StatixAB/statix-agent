use std::{
    env,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::{
    process::Command as TokioCommand,
    time::{Duration, timeout},
};

use crate::{
    config::agent_state_dir,
    jobs::{ExecutionContext, JobExecutionResult, PreparedWorkspace, Runner, summarize_command_output},
};

const DEFAULT_CPU_COUNT: u8 = 2;
const DEFAULT_MEMORY_MB: u32 = 4096;
const WORKSPACE_ARCHIVE: &str = "statix-workspace.tar.gz";

pub struct ContainerRunner {
    image: String,
    cpu: Option<u8>,
    memory_mb: Option<u32>,
}

impl ContainerRunner {
    pub fn new(image: String, cpu: Option<u8>, memory_mb: Option<u32>) -> Self {
        Self { image, cpu, memory_mb }
    }
}

#[async_trait::async_trait]
impl Runner for ContainerRunner {
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

        let image = LxcImage::parse(&self.image)?;
        let container_name = container_name(&ctx.attempt_id);
        let cpu = self.cpu.unwrap_or(DEFAULT_CPU_COUNT);
        let memory_mb = self.memory_mb.unwrap_or(DEFAULT_MEMORY_MB);
        let runtime_root = agent_state_dir()?
            .join("container")
            .join(&ctx.job_id)
            .join(&ctx.attempt_id);
        fs::create_dir_all(&runtime_root).with_context(|| format!("failed to create {}", runtime_root.display()))?;

        eprintln!(
            "[statix-agent] job {}: preparing lxc container {} from {}",
            ctx.job_id, container_name, self.image
        );
        eprintln!(
            "[statix-agent] job {}: requested container limits: {} cpu(s), {} MiB memory",
            ctx.job_id, cpu, memory_mb
        );

        let workspace_tar = runtime_root.join(WORKSPACE_ARCHIVE);
        create_workspace_archive(&workspace_tar, &workspace.workdir).await?;
        eprintln!(
            "[statix-agent] job {}: archived workspace {}",
            ctx.job_id,
            workspace.workdir.display()
        );

        let mut container = LxcContainer::create(container_name.clone(), image, cpu, memory_mb).await?;
        let result = async {
            container.start().await?;
            container.copy_archive_to_guest(&workspace_tar).await?;
            if let Some(result) = container.prepare_guest(ctx.timeout_seconds, workspace).await? {
                return Ok(result);
            }
            container.run_command(ctx.timeout_seconds, command, workspace).await
        }
        .await;

        eprintln!(
            "[statix-agent] job {}: destroying lxc container {}",
            ctx.job_id, container_name
        );
        container.destroy().await;

        result
    }
}

struct LxcContainer {
    name: String,
    destroyed: bool,
}

impl LxcContainer {
    async fn create(name: String, image: LxcImage, cpu: u8, memory_mb: u32) -> Result<Self> {
        ensure_lxc_directory_permissions()?;

        let lxc_path = lxc_storage_path();
        fs::create_dir_all(&lxc_path)
            .with_context(|| format!("failed to create {}", lxc_path.display()))?;
        set_traversable_directory(&lxc_path)?;

        let log_path = lxc_path.join(format!("{name}.create.log"));
        let status = lxc_command("lxc-create")
            .arg("-n")
            .arg(&name)
            .arg("-P")
            .arg(&lxc_path)
            .arg("--logfile")
            .arg(&log_path)
            .arg("--logpriority")
            .arg("DEBUG")
            .arg("-t")
            .arg("download")
            .arg("--")
            .arg("-d")
            .arg(&image.distribution)
            .arg("-r")
            .arg(&image.release)
            .arg("-a")
            .arg(lxc_arch())
            .status()
            .await
            .with_context(|| missing_dependency_message("lxc-create", "lxc"))?;

        if !status.success() {
            bail!(
                "lxc-create failed for container {name} with {status}: {}",
                lxc_log_excerpt(&log_path)
            );
        }

        let container = Self { name, destroyed: false };
        container.apply_job_config(cpu, memory_mb, enforce_lxc_limits())?;
        Ok(container)
    }

    async fn start(&mut self) -> Result<()> {
        let log_path = self.log_path();
        let status = lxc_command("lxc-start")
            .arg("-n")
            .arg(&self.name)
            .arg("-P")
            .arg(lxc_storage_path())
            .arg("--logfile")
            .arg(&log_path)
            .arg("--logpriority")
            .arg("DEBUG")
            .arg("-d")
            .status()
            .await
            .with_context(|| missing_dependency_message("lxc-start", "lxc"))?;

        if !status.success() {
            let excerpt = lxc_log_excerpt(&log_path);
            bail!(
                "lxc-start failed for container {} with {status}: {}",
                self.name,
                lxc_start_failure_message(&excerpt)
            );
        }

        let status = lxc_command("lxc-wait")
            .arg("-n")
            .arg(&self.name)
            .arg("-P")
            .arg(lxc_storage_path())
            .arg("-s")
            .arg("RUNNING")
            .arg("-t")
            .arg("30")
            .status()
            .await
            .with_context(|| missing_dependency_message("lxc-wait", "lxc"))?;

        if !status.success() {
            bail!(
                "lxc-wait did not observe container {} running: {status}: {}",
                self.name,
                lxc_log_excerpt(&log_path)
            );
        }

        Ok(())
    }

    async fn copy_archive_to_guest(&self, archive_path: &Path) -> Result<()> {
        let destination = self.rootfs_path().join("tmp").join(WORKSPACE_ARCHIVE);
        fs::copy(archive_path, &destination).with_context(|| {
            format!(
                "failed to copy workspace archive into lxc rootfs at {}",
                destination.display()
            )
        })?;
        Ok(())
    }

    async fn prepare_guest(
        &self,
        timeout_seconds: u64,
        workspace: &PreparedWorkspace,
    ) -> Result<Option<JobExecutionResult>> {
        let setup_command = "command -v cargo >/dev/null 2>&1 || (apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y build-essential ca-certificates cargo git libssl-dev pkg-config)";
        let output = self.attach_output(timeout_seconds, setup_command).await?;
        if !output.status.success() {
            let message = summarize_command_output(&workspace.workdir, &output.stdout, &output.stderr);
            eprintln!(
                "[statix-agent] lxc container setup failed with {}; output: {}",
                output.status,
                truncate_for_log(&message, 1_000)
            );
            return Ok(Some(JobExecutionResult {
                status: "failed",
                message: Some(message),
            }));
        }
        Ok(None)
    }

    async fn run_command(
        &self,
        timeout_seconds: u64,
        command: &[String],
        workspace: &PreparedWorkspace,
    ) -> Result<JobExecutionResult> {
        let guest_command = format!(
            "rm -rf /workspace && mkdir -p /workspace && tar -xzf /tmp/{archive} -C /workspace && cd /workspace && exec {command}",
            archive = WORKSPACE_ARCHIVE,
            command = shell_join(command)
        );

        eprintln!(
            "[statix-agent] running command inside lxc container {}: {}",
            self.name,
            shell_join(command)
        );
        let output = self.attach_output(timeout_seconds, &guest_command).await?;
        let message = summarize_command_output(&workspace.workdir, &output.stdout, &output.stderr);

        if output.status.success() {
            eprintln!("[statix-agent] lxc container command succeeded");
            Ok(JobExecutionResult { status: "succeeded", message: Some(message) })
        } else {
            eprintln!(
                "[statix-agent] lxc container command failed with {}; output: {}",
                output.status,
                truncate_for_log(&message, 1_000)
            );
            Ok(JobExecutionResult { status: "failed", message: Some(message) })
        }
    }

    async fn attach_output(&self, timeout_seconds: u64, shell_command: &str) -> Result<std::process::Output> {
        let mut process = lxc_command("lxc-attach");
        process
            .arg("-n")
            .arg(&self.name)
            .arg("-P")
            .arg(lxc_storage_path())
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg(shell_command)
            .kill_on_drop(true);

        timeout(Duration::from_secs(timeout_seconds), process.output())
            .await
            .map_err(|_| anyhow!("lxc command timed out after {} seconds", timeout_seconds))?
            .map_err(anyhow::Error::from)
            .map_err(|error| error.context(format!("failed to execute command inside lxc container {}", self.name)))
    }

    async fn destroy(&mut self) {
        if self.destroyed {
            return;
        }

        let _ = lxc_command("lxc-stop")
            .arg("-n")
            .arg(&self.name)
            .arg("-P")
            .arg(lxc_storage_path())
            .arg("--kill")
            .status()
            .await;
        let _ = lxc_command("lxc-destroy")
            .arg("-n")
            .arg(&self.name)
            .arg("-P")
            .arg(lxc_storage_path())
            .status()
            .await;
        self.destroyed = true;
    }

    fn rootfs_path(&self) -> PathBuf {
        lxc_storage_path().join(&self.name).join("rootfs")
    }

    fn config_path(&self) -> PathBuf {
        lxc_storage_path().join(&self.name).join("config")
    }

    fn log_path(&self) -> PathBuf {
        lxc_storage_path().join(&self.name).join("statix-lxc.log")
    }

    fn apply_job_config(&self, cpu: u8, memory_mb: u32, enforce_limits: bool) -> Result<()> {
        let mut config = fs::OpenOptions::new()
            .append(true)
            .open(self.config_path())
            .with_context(|| format!("failed to open lxc config for {}", self.name))?;

        write_job_lxc_config(&mut config, cpu, memory_mb, enforce_limits)?;
        Ok(())
    }
}

impl Drop for LxcContainer {
    fn drop(&mut self) {
        if !self.destroyed {
            eprintln!(
                "[statix-agent] lxc container {} was not destroyed before drop; cleanup may be needed",
                self.name
            );
        }
    }
}

struct LxcImage {
    distribution: String,
    release: String,
}

impl LxcImage {
    fn parse(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            bail!("container image must not be empty");
        }

        let Some((distribution, release)) = trimmed.split_once(':') else {
            bail!("container image must use distribution:release format, for example ubuntu:24.04");
        };
        let distribution = distribution.trim();
        let release = release.trim();
        if distribution.is_empty() || release.is_empty() {
            bail!("container image must use distribution:release format, for example ubuntu:24.04");
        }

        Ok(Self {
            distribution: distribution.to_string(),
            release: normalize_release(distribution, release).to_string(),
        })
    }
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
        bail!("failed to archive workspace for container execution");
    }

    Ok(())
}

fn lxc_command(program: &str) -> TokioCommand {
    let mut command = TokioCommand::new(program);
    if let Some(home) = lxc_process_home() {
        command.env("HOME", &home);
        command.env("XDG_CACHE_HOME", home.join(".cache"));
        command.env("XDG_CONFIG_HOME", home.join(".config"));
        command.env("XDG_DATA_HOME", home.join(".local").join("share"));
    }
    command.kill_on_drop(true);
    command
}

fn lxc_process_home() -> Option<PathBuf> {
    env_path("STATIX_AGENT_STATE_DIR")
        .or_else(|| env_path("STATE_DIRECTORY"))
        .map(|path| path.join("lxc"))
}

fn lxc_storage_path() -> PathBuf {
    lxc_process_home()
        .map(|path| path.join("containers"))
        .unwrap_or_else(|| PathBuf::from("/var/lib/lxc"))
}

fn ensure_lxc_directory_permissions() -> Result<()> {
    let Some(home) = lxc_process_home() else {
        return Ok(());
    };

    if let Some(state_dir) = home.parent() {
        set_traversable_directory(state_dir)?;
    }
    fs::create_dir_all(&home).with_context(|| format!("failed to create {}", home.display()))?;
    set_traversable_directory(&home)?;
    Ok(())
}

fn set_traversable_directory(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    if !metadata.is_dir() {
        return Ok(());
    }

    let mut permissions = metadata.permissions();
    let mode = permissions.mode();
    let traversable_mode = mode | 0o711;
    if mode != traversable_mode {
        permissions.set_mode(traversable_mode);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn normalize_release<'a>(distribution: &str, release: &'a str) -> &'a str {
    match (distribution, release) {
        ("ubuntu", "24.04") => "noble",
        ("ubuntu", "22.04") => "jammy",
        ("ubuntu", "20.04") => "focal",
        _ => release,
    }
}

fn lxc_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        arch => arch,
    }
}

fn write_job_lxc_config(mut writer: impl Write, cpu: u8, memory_mb: u32, enforce_limits: bool) -> Result<()> {
    writeln!(writer, "\n# Statix job runtime config")?;
    if enforce_limits {
        let memory_bytes = u64::from(memory_mb) * 1024 * 1024;
        let cpu_quota = u64::from(cpu) * 100_000;
        writeln!(writer, "lxc.cgroup2.memory.max = {memory_bytes}")?;
        writeln!(writer, "lxc.cgroup2.cpu.max = {cpu_quota} 100000")?;
    } else {
        writeln!(
            writer,
            "# cgroup limits requested by Statix are not written by default because unprivileged LXC startup fails on hosts without a fully delegated writable cgroup subtree."
        )?;
    }
    writeln!(writer, "lxc.apparmor.profile = unconfined")?;
    Ok(())
}

fn enforce_lxc_limits() -> bool {
    env::var("STATIX_LXC_ENFORCE_LIMITS")
        .ok()
        .map(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn container_name(attempt_id: &str) -> String {
    let suffix = attempt_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    let suffix = if suffix.is_empty() { "job".to_string() } else { suffix };
    format!("statix-{}", truncate_for_name(&suffix, 56))
}

fn truncate_for_name(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
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

fn truncate_for_log(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let mut shortened = value.chars().take(max_chars).collect::<String>();
    shortened.push_str("...");
    shortened
}

fn lxc_log_excerpt(path: &Path) -> String {
    match fs::read_to_string(path) {
        Ok(log) => {
            let log = log.trim();
            if log.is_empty() {
                format!("lxc log {} was empty", path.display())
            } else {
                tail_for_log(log, 4_000)
            }
        }
        Err(error) => format!("failed to read lxc log {}: {error}", path.display()),
    }
}

fn lxc_start_failure_message(log_excerpt: &str) -> String {
    lxc_start_failure_message_with_host_context(log_excerpt, proc_mount_uses_noatime())
}

fn lxc_start_failure_message_with_host_context(log_excerpt: &str, proc_uses_noatime: Option<bool>) -> String {
    if log_excerpt.contains("Failed to mount \"proc\"")
        && log_excerpt.contains("/usr/lib/")
        && log_excerpt.contains("/lxc/rootfs/proc")
        && log_excerpt.contains("Operation not permitted")
    {
        let mut message = format!(
            "{log_excerpt}\nHint: LXC needs write access to its package rootfs mountpoint under /usr/lib/*/lxc/rootfs. If statix-agent is running under systemd with ProtectSystem=strict, add ReadWritePaths for the distro multiarch LXC rootfs path and restart the service."
        );
        match proc_uses_noatime {
            Some(true) => message.push_str(
                " The statix-agent process also sees /proc mounted with noatime; unprivileged LXC can fail to mount proc from that parent mount. Remount /proc with relatime and make the matching /etc/fstab change persistent.",
            ),
            Some(false) => message.push_str(
                " If ReadWritePaths is already applied, check the host /proc mount options with `findmnt -no OPTIONS /proc`; unprivileged LXC can fail when /proc is mounted with noatime instead of relatime.",
            ),
            None => message.push_str(
                " If ReadWritePaths is already applied, check the host /proc mount options; unprivileged LXC can fail when /proc is mounted with noatime instead of relatime.",
            ),
        }
        message
    } else {
        log_excerpt.to_string()
    }
}

fn proc_mount_uses_noatime() -> Option<bool> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut found_proc = false;

    for line in mountinfo.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.get(4) != Some(&"/proc") {
            continue;
        }
        found_proc = true;

        if fields
            .get(5)
            .map(|options| options.split(',').any(|option| option == "noatime"))
            .unwrap_or(false)
        {
            return Some(true);
        }
    }

    found_proc.then_some(false)
}

fn tail_for_log(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let tail = value
        .chars()
        .rev()
        .take(max_chars)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("...{tail}")
}

fn missing_dependency_message(program: &str, debian_package: &str) -> String {
    format!(
        "failed to launch {program}; install the '{debian_package}' package and ensure {program} is on PATH"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ubuntu_version_alias() {
        let image = LxcImage::parse("ubuntu:24.04").expect("image should parse");
        assert_eq!(image.distribution, "ubuntu");
        assert_eq!(image.release, "noble");
    }

    #[test]
    fn parses_named_release_without_alias() {
        let image = LxcImage::parse("debian:bookworm").expect("image should parse");
        assert_eq!(image.distribution, "debian");
        assert_eq!(image.release, "bookworm");
    }

    #[test]
    fn rejects_images_without_release() {
        assert!(LxcImage::parse("ubuntu").is_err());
    }

    #[test]
    fn sanitizes_container_names() {
        assert_eq!(container_name("01KQ9/ABC_def"), "statix-01kq9-abc-def");
    }

    #[test]
    fn writes_job_lxc_config_without_limits_by_default() {
        let mut config = Vec::new();
        write_job_lxc_config(&mut config, 2, 4096, false).expect("config should be written");
        let config = String::from_utf8(config).expect("config should be utf8");

        assert!(!config.contains("lxc.cgroup2.memory.max"));
        assert!(!config.contains("lxc.cgroup2.cpu.max"));
        assert!(config.contains("cgroup limits requested by Statix are not written by default"));
        assert!(config.contains("lxc.apparmor.profile = unconfined"));
    }

    #[test]
    fn writes_job_lxc_config_with_opt_in_limits() {
        let mut config = Vec::new();
        write_job_lxc_config(&mut config, 2, 4096, true).expect("config should be written");
        let config = String::from_utf8(config).expect("config should be utf8");

        assert!(config.contains("lxc.cgroup2.memory.max = 4294967296"));
        assert!(config.contains("lxc.cgroup2.cpu.max = 200000 100000"));
        assert!(config.contains("lxc.apparmor.profile = unconfined"));
    }

    #[test]
    fn quotes_shell_arguments() {
        assert_eq!(
            shell_join(&["cargo".to_string(), "test".to_string(), "a b".to_string()]),
            "cargo test 'a b'"
        );
    }

    #[test]
    fn truncates_lxc_logs_from_the_tail() {
        assert_eq!(tail_for_log("abcdef", 3), "...def");
        assert_eq!(tail_for_log("abc", 3), "abc");
    }

    #[test]
    fn adds_hint_for_lxc_proc_mount_denial_under_package_rootfs() {
        let log = "Operation not permitted - Failed to mount \"proc\" onto \"/usr/lib/x86_64-linux-gnu/lxc/rootfs/proc\"";
        let message = lxc_start_failure_message_with_host_context(log, Some(false));

        assert!(message.contains(log));
        assert!(message.contains("ReadWritePaths"));
        assert!(message.contains("/usr/lib/*/lxc/rootfs"));
        assert!(message.contains("findmnt -no OPTIONS /proc"));
    }

    #[test]
    fn adds_noatime_hint_when_proc_parent_mount_uses_noatime() {
        let log = "Operation not permitted - Failed to mount \"proc\" onto \"/usr/lib/x86_64-linux-gnu/lxc/rootfs/proc\"";
        let message = lxc_start_failure_message_with_host_context(log, Some(true));

        assert!(message.contains("noatime"));
        assert!(message.contains("relatime"));
    }
}
