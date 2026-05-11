use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command as TokioCommand;

use crate::networking::VmNetworkLease;

use super::util::{
    DEFAULT_SSH_USER, canonical_ubuntu_image_url, image_cache_key, missing_dependency_message,
    safe_path_segment,
};

pub(super) async fn resolve_base_image(image: &str, runtime_root: &Path) -> Result<PathBuf> {
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
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create {}", cache_dir.display()))?;
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

    fs::write(&cached_image, &bytes).with_context(|| {
        format!(
            "failed to cache microvm image at {}",
            cached_image.display()
        )
    })?;
    Ok(cached_image)
}

pub(super) async fn create_overlay_disk(base_image: &Path, overlay_image: &Path) -> Result<()> {
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

pub(super) async fn generate_ssh_keypair(private_key: &Path, public_key: &Path) -> Result<()> {
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
        bail!("failed to archive workspace for microvm execution");
    }

    Ok(())
}

pub(super) async fn create_cloud_init_seed(
    seed_iso: &Path,
    public_key: &Path,
    instance_id: &str,
    network: Option<&VmNetworkLease>,
) -> Result<()> {
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
"#,
        user = DEFAULT_SSH_USER,
        public_key = public_key.trim(),
    );

    let meta_data = format!(
        "instance-id: statix-{instance_id}\nlocal-hostname: statix-microvm\n",
        instance_id = safe_path_segment(instance_id)
    );
    let user_data_path = seed_iso.with_extension("user-data");
    let meta_data_path = seed_iso.with_extension("meta-data");
    let network_config_path = seed_iso.with_extension("network-config");
    fs::write(&user_data_path, user_data)
        .with_context(|| format!("failed to write {}", user_data_path.display()))?;
    fs::write(&meta_data_path, meta_data)
        .with_context(|| format!("failed to write {}", meta_data_path.display()))?;

    let mut command = TokioCommand::new("cloud-localds");
    command.arg(seed_iso);
    if let Some(network) = network {
        let network_config = format!(
            "version: 2\nethernets:\n  statix0:\n    match:\n      macaddress: \"{}\"\n    set-name: statix0\n    addresses:\n      - {}/{}\n    gateway4: {}\n    nameservers:\n      addresses:\n        - {}\n",
            network.mac_address,
            network.vm_ip,
            network.prefix_len,
            network.gateway,
            network.gateway
        );
        fs::write(&network_config_path, network_config)
            .with_context(|| format!("failed to write {}", network_config_path.display()))?;
        command.arg("--network-config").arg(&network_config_path);
    }

    let status = command
        .arg(&user_data_path)
        .arg(&meta_data_path)
        .status()
        .await
        .with_context(|| missing_dependency_message("cloud-localds", "cloud-image-utils"))?;

    let _ = fs::remove_file(user_data_path);
    let _ = fs::remove_file(meta_data_path);
    let _ = fs::remove_file(network_config_path);

    if !status.success() {
        bail!("cloud-localds failed to create the seed image");
    }

    Ok(())
}
