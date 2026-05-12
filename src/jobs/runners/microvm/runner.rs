use std::fs;

use anyhow::{Context, Result, bail};

use crate::config::agent_state_dir;
use crate::jobs::{ExecutionContext, JobExecutionResult, PreparedWorkspace, Runner};
use crate::logs;
use crate::networking;

use super::guest::{
    emit_job_stderr, guest_ready, run_guest_command, wait_for_guest_ready,
    wait_for_project_guest_ready,
};
use super::image::{
    create_cloud_init_seed, create_overlay_disk, create_workspace_archive,
    ensure_overlay_disk_min_size, generate_ssh_keypair, resolve_base_image,
};
use super::qemu::{
    desired_project_qemu_command, launch_project_qemu, launch_qemu, project_qemu_command,
    stop_project_qemu_process, stop_project_qemu_process_on_ssh_port,
};
use super::util::{
    DEFAULT_CPU_COUNT, DEFAULT_MEMORY_MB, DEFAULT_SSH_PORT, project_vm_key, project_vm_ssh_port,
    safe_path_segment,
};

const PROJECT_DISK_GENERATION: &str = "disk-v2";

pub struct MicrovmRunner {
    image: String,
    cpu: Option<u8>,
    memory_mb: Option<u32>,
}

pub struct ProjectMicrovmRunner {
    project_id: String,
    environment: String,
    image: String,
    cpu: Option<u8>,
    memory_mb: Option<u32>,
}

impl MicrovmRunner {
    pub fn new(image: String, cpu: Option<u8>, memory_mb: Option<u32>) -> Self {
        Self {
            image,
            cpu,
            memory_mb,
        }
    }
}

impl ProjectMicrovmRunner {
    pub fn new(
        project_id: String,
        environment: String,
        image: String,
        cpu: Option<u8>,
        memory_mb: Option<u32>,
    ) -> Self {
        Self {
            project_id,
            environment,
            image,
            cpu,
            memory_mb,
        }
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

        let runtime_root = agent_state_dir()?
            .join("microvm")
            .join(&ctx.job_id)
            .join(&ctx.attempt_id);
        fs::create_dir_all(&runtime_root)
            .with_context(|| format!("failed to create {}", runtime_root.display()))?;
        logs::job_phase_info(
            &ctx.job_id,
            "microvm.setup",
            format!("preparing runtime at {}", runtime_root.display()),
        );

        let base_image = resolve_base_image(&self.image, &runtime_root).await?;
        logs::job_phase_info(
            &ctx.job_id,
            "microvm.setup",
            format!("using base image {}", base_image.display()),
        );
        let overlay_image = runtime_root.join("disk.qcow2");
        create_overlay_disk(&base_image, &overlay_image).await?;
        logs::job_phase_info(
            &ctx.job_id,
            "microvm.setup",
            format!("created overlay disk {}", overlay_image.display()),
        );

        let ssh_dir = runtime_root.join("ssh");
        fs::create_dir_all(&ssh_dir)
            .with_context(|| format!("failed to create {}", ssh_dir.display()))?;
        let private_key = ssh_dir.join("id_ed25519");
        let public_key = ssh_dir.join("id_ed25519.pub");
        generate_ssh_keypair(&private_key, &public_key).await?;
        logs::job_phase_info(&ctx.job_id, "microvm.setup", "generated ssh keypair");

        let seed_iso = runtime_root.join("seed.iso");
        let workspace_tar = runtime_root.join("workspace.tar.gz");
        create_workspace_archive(&workspace_tar, &workspace.workdir).await?;
        logs::job_phase_info(
            &ctx.job_id,
            "microvm.setup",
            format!("archived workspace {}", workspace.workdir.display()),
        );
        create_cloud_init_seed(&seed_iso, &public_key, &ctx.attempt_id, None).await?;
        logs::job_phase_info(
            &ctx.job_id,
            "microvm.setup",
            format!("created cloud-init seed {}", seed_iso.display()),
        );

        let cpu = self.cpu.unwrap_or(DEFAULT_CPU_COUNT);
        let memory_mb = self.memory_mb.unwrap_or(DEFAULT_MEMORY_MB);
        let mut qemu =
            launch_qemu(&overlay_image, &seed_iso, cpu, memory_mb, DEFAULT_SSH_PORT).await?;
        logs::job_phase_info(
            &ctx.job_id,
            "microvm.runtime",
            format!(
                "launched with {} cpu(s), {} MiB memory, ssh port {}",
                cpu, memory_mb, DEFAULT_SSH_PORT
            ),
        );

        let result = match wait_for_guest_ready(
            &ctx.job_id,
            &mut qemu,
            DEFAULT_SSH_PORT,
            &private_key,
            ctx.timeout_seconds,
        )
        .await
        {
            Ok(()) => {
                logs::job_phase_info(&ctx.job_id, "microvm.runtime", "guest is ready");
                run_guest_command(
                    ctx,
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
                logs::job_phase_error(
                    &ctx.job_id,
                    "microvm.runtime",
                    format!("readiness failed: {error:#}"),
                );
                Err(error)
            }
        };

        logs::job_phase_info(&ctx.job_id, "microvm.runtime", "shutting down");
        qemu.shutdown().await;

        result
    }
}

#[async_trait::async_trait]
impl Runner for ProjectMicrovmRunner {
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

        let vm_key = project_vm_key(&self.project_id, &self.environment);
        let runtime_root = agent_state_dir()?
            .join("microvm")
            .join("projects")
            .join(&vm_key);
        let runtime_root_was_missing = !runtime_root.exists();
        fs::create_dir_all(&runtime_root)
            .with_context(|| format!("failed to create {}", runtime_root.display()))?;
        emit_job_stderr(
            ctx,
            format!(
                "preparing project microvm {} at {}",
                vm_key,
                runtime_root.display()
            ),
        );

        let base_image = resolve_base_image(&self.image, &runtime_root).await?;
        let overlay_image = runtime_root.join("disk.qcow2");
        let disk_generation_path = runtime_root.join("disk.generation");
        let disk_generation = fs::read_to_string(&disk_generation_path)
            .ok()
            .map(|value| value.trim().to_string());
        if overlay_image.exists() && disk_generation.as_deref() != Some(PROJECT_DISK_GENERATION) {
            stop_project_qemu_process(&runtime_root);
            let backup_image = runtime_root.join(format!("disk.{}.qcow2", PROJECT_DISK_GENERATION));
            let _ = fs::remove_file(&backup_image);
            fs::rename(&overlay_image, &backup_image).with_context(|| {
                format!(
                    "failed to move existing project microvm disk {} to {}",
                    overlay_image.display(),
                    backup_image.display()
                )
            })?;
            emit_job_stderr(
                ctx,
                format!(
                    "backed up previous project microvm disk to {}; creating {} disk",
                    backup_image.display(),
                    PROJECT_DISK_GENERATION
                ),
            );
        }
        if !overlay_image.exists() {
            create_overlay_disk(&base_image, &overlay_image).await?;
        } else if disk_generation.as_deref() != Some(PROJECT_DISK_GENERATION) {
            ensure_overlay_disk_min_size(&overlay_image).await?;
        }
        fs::write(&disk_generation_path, PROJECT_DISK_GENERATION)
            .with_context(|| format!("failed to write {}", disk_generation_path.display()))?;

        let ssh_dir = runtime_root.join("ssh");
        fs::create_dir_all(&ssh_dir)
            .with_context(|| format!("failed to create {}", ssh_dir.display()))?;
        let private_key = ssh_dir.join("id_ed25519");
        let public_key = ssh_dir.join("id_ed25519.pub");
        if !private_key.exists() || !public_key.exists() {
            generate_ssh_keypair(&private_key, &public_key).await?;
        }

        let ssh_port = project_vm_ssh_port(&vm_key);
        if runtime_root_was_missing {
            let stopped = stop_project_qemu_process_on_ssh_port(ssh_port);
            if stopped > 0 {
                emit_job_stderr(
                    ctx,
                    format!(
                        "stopped {stopped} orphaned project microvm process(es) on ssh port {ssh_port}"
                    ),
                );
            }
        }
        let network = networking::ensure_project_network(
            &self.project_id,
            &self.environment,
            &vm_key,
            &ctx.exposure,
        )?;
        emit_job_stderr(
            ctx,
            format!(
                "project microvm network: vm_ip={}/{} tap={} mac={} ssh=127.0.0.1:{}",
                network.vm_ip, network.prefix_len, network.tap_name, network.mac_address, ssh_port
            ),
        );
        let routes = networking::project_route_descriptions(&self.project_id, &self.environment)?;
        if routes.is_empty() {
            emit_job_stderr(ctx, "project microvm routes: no exposures requested");
        }
        for route in routes {
            emit_job_stderr(ctx, format!("project microvm route: {route}"));
        }
        let seed_iso = runtime_root.join("seed.iso");
        let seed_instance_id = format!("{vm_key}-{PROJECT_DISK_GENERATION}");
        create_cloud_init_seed(&seed_iso, &public_key, &seed_instance_id, Some(&network)).await?;

        let cpu = self.cpu.unwrap_or(DEFAULT_CPU_COUNT);
        let memory_mb = self.memory_mb.unwrap_or(DEFAULT_MEMORY_MB);
        let desired_qemu_command = desired_project_qemu_command(
            &overlay_image,
            &seed_iso,
            cpu,
            memory_mb,
            ssh_port,
            Some(&network),
        )?;
        let qemu_command_changed = project_qemu_command(&runtime_root)
            .is_some_and(|command| command != desired_qemu_command);
        if qemu_command_changed {
            emit_job_stderr(
                ctx,
                "project microvm qemu command changed; relaunching runtime",
            );
        }

        if qemu_command_changed || !guest_ready(ssh_port, &private_key).await {
            stop_project_qemu_process(&runtime_root);
            let stopped = stop_project_qemu_process_on_ssh_port(ssh_port);
            if stopped > 0 {
                emit_job_stderr(
                    ctx,
                    format!(
                        "stopped {stopped} orphaned project microvm process(es) on ssh port {ssh_port}"
                    ),
                );
            }
            let pid = launch_project_qemu(
                &runtime_root,
                &overlay_image,
                &seed_iso,
                cpu,
                memory_mb,
                ssh_port,
                Some(&network),
            )
            .await?;
            emit_job_stderr(
                ctx,
                format!(
                    "launched project microvm pid {} with {} cpu(s), {} MiB memory, ssh port {}",
                    pid, cpu, memory_mb, ssh_port
                ),
            );
            if let Some(command) = project_qemu_command(&runtime_root) {
                emit_job_stderr(ctx, format!("project microvm qemu command: {command}"));
            }
        }

        wait_for_project_guest_ready(
            ctx,
            &runtime_root,
            ssh_port,
            &private_key,
            ctx.timeout_seconds,
        )
        .await?;
        emit_job_stderr(ctx, "project microvm guest is ready");

        let workspace_tar = runtime_root.join(format!(
            "workspace-{}.tar.gz",
            safe_path_segment(&ctx.attempt_id)
        ));
        create_workspace_archive(&workspace_tar, &workspace.workdir).await?;

        let result = run_guest_command(
            ctx,
            ssh_port,
            &private_key,
            ctx.timeout_seconds,
            command,
            workspace,
            &workspace_tar,
        )
        .await;

        let _ = fs::remove_file(workspace_tar);
        result
    }
}
