use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use tokio::process::Command as TokioCommand;

pub(super) const WORKSPACE_ARCHIVE: &str = "statix-workspace.tar.gz";

pub(super) async fn create_workspace_archive(archive_path: &Path, workdir: &Path) -> Result<()> {
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
