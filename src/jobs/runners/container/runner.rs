use std::fs;

use anyhow::{Context, Result, bail};

use crate::{
    config::agent_state_dir,
    jobs::{ExecutionContext, JobExecutionResult, PreparedWorkspace, Runner},
};

use super::{
    archive::{WORKSPACE_ARCHIVE, create_workspace_archive},
    image::LxcImage,
    lxc::LxcContainer,
    shell::container_name,
};

const DEFAULT_CPU_COUNT: u8 = 2;
const DEFAULT_MEMORY_MB: u32 = 4096;

pub struct ContainerRunner {
    image: String,
    cpu: Option<u8>,
    memory_mb: Option<u32>,
}

impl ContainerRunner {
    pub fn new(image: String, cpu: Option<u8>, memory_mb: Option<u32>) -> Self {
        Self {
            image,
            cpu,
            memory_mb,
        }
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
        fs::create_dir_all(&runtime_root)
            .with_context(|| format!("failed to create {}", runtime_root.display()))?;

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

        let mut container = LxcContainer::create(
            container_name.clone(),
            image.distribution.as_str(),
            image.release.as_str(),
            cpu,
            memory_mb,
        )
        .await?;

        let result = async {
            container.start().await?;
            container
                .configure_guest_network(ctx.timeout_seconds)
                .await?;
            container.configure_guest_dns(ctx.timeout_seconds).await?;
            if let Some(result) = container
                .prepare_guest(ctx, ctx.timeout_seconds, workspace)
                .await?
            {
                return Ok(result);
            }
            container.copy_archive_to_guest(&workspace_tar).await?;
            container
                .run_command(ctx, ctx.timeout_seconds, command, workspace)
                .await
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
