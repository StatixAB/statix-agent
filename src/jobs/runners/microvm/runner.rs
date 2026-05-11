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
    create_cloud_init_seed, create_overlay_disk, create_workspace_archive, generate_ssh_keypair,
    resolve_base_image,
};
use super::qemu::{launch_project_qemu, launch_qemu, stop_project_qemu_process};
use super::util::{
    DEFAULT_CPU_COUNT, DEFAULT_MEMORY_MB, DEFAULT_SSH_PORT, project_vm_key, project_vm_ssh_port,
    safe_path_segment,
};

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
        if !overlay_image.exists() {
            create_overlay_disk(&base_image, &overlay_image).await?;
        }

        let ssh_dir = runtime_root.join("ssh");
        fs::create_dir_all(&ssh_dir)
            .with_context(|| format!("failed to create {}", ssh_dir.display()))?;
        let private_key = ssh_dir.join("id_ed25519");
        let public_key = ssh_dir.join("id_ed25519.pub");
        if !private_key.exists() || !public_key.exists() {
            generate_ssh_keypair(&private_key, &public_key).await?;
        }

        let ssh_port = project_vm_ssh_port(&vm_key);
        let network = networking::ensure_project_network(
            &self.project_id,
            &self.environment,
            &vm_key,
            &ctx.exposure,
        )?;
        let seed_iso = runtime_root.join("seed.iso");
        create_cloud_init_seed(&seed_iso, &public_key, &vm_key, Some(&network)).await?;

        let cpu = self.cpu.unwrap_or(DEFAULT_CPU_COUNT);
        let memory_mb = self.memory_mb.unwrap_or(DEFAULT_MEMORY_MB);
        if !guest_ready(ssh_port, &private_key).await {
            stop_project_qemu_process(&runtime_root);
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
