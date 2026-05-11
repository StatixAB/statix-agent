use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};

use crate::logs;
use crate::networking::VmNetworkLease;

use super::util::{
    missing_dependency_message, process_is_running, qemu_binary, qemu_package_hint, shell_join,
    truncate_for_log,
};

pub(super) const HOSTFWD_NET_MAC_ADDRESS: &str = "52:54:00:12:34:56";

pub(super) struct QemuProcess {
    child: Option<Child>,
    recent_logs: Arc<Mutex<VecDeque<String>>>,
}

impl QemuProcess {
    pub(super) fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };

        let status = child.try_wait().context("failed to poll qemu status")?;
        if status.is_some() {
            self.child.take();
        }

        Ok(status)
    }

    pub(super) fn failure_message(&self, status: std::process::ExitStatus) -> String {
        let logs = self
            .recent_logs
            .lock()
            .ok()
            .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();

        if logs.trim().is_empty() {
            format!("qemu exited before the guest became ready: {status}")
        } else {
            format!(
                "qemu exited before the guest became ready: {status}\nrecent qemu logs:\n{logs}"
            )
        }
    }

    pub(super) async fn shutdown(&mut self) {
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

pub(super) async fn launch_qemu(
    overlay_image: &Path,
    seed_iso: &Path,
    cpu: u8,
    memory_mb: u32,
    ssh_port: u16,
) -> Result<QemuProcess> {
    let binary = qemu_binary()?;
    let recent_logs = Arc::new(Mutex::new(VecDeque::with_capacity(32)));
    let args = qemu_launch_args(overlay_image, seed_iso, cpu, memory_mb, ssh_port, None);
    logs::agent_debug(format!("launching qemu: {binary} {}", shell_join(&args)));
    let mut command = Command::new(binary);
    command.args(args);

    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| missing_dependency_message(binary, qemu_package_hint()))?;
    spawn_qemu_log_stream("stdout", child.stdout.take(), Arc::clone(&recent_logs));
    spawn_qemu_log_stream("stderr", child.stderr.take(), Arc::clone(&recent_logs));
    Ok(QemuProcess {
        child: Some(child),
        recent_logs,
    })
}

pub(super) async fn launch_project_qemu(
    runtime_root: &Path,
    overlay_image: &Path,
    seed_iso: &Path,
    cpu: u8,
    memory_mb: u32,
    ssh_port: u16,
    network: Option<&VmNetworkLease>,
) -> Result<u32> {
    let binary = qemu_binary()?;
    let args = qemu_launch_args(overlay_image, seed_iso, cpu, memory_mb, ssh_port, network);
    fs::write(
        runtime_root.join("qemu.command.txt"),
        format_qemu_command(binary, &args),
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            runtime_root.join("qemu.command.txt").display()
        )
    })?;
    let stdout = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(runtime_root.join("qemu.stdout.log"))
        .with_context(|| {
            format!(
                "failed to open {}",
                runtime_root.join("qemu.stdout.log").display()
            )
        })?;
    let stderr = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(runtime_root.join("qemu.stderr.log"))
        .with_context(|| {
            format!(
                "failed to open {}",
                runtime_root.join("qemu.stderr.log").display()
            )
        })?;
    let child = Command::new(binary)
        .args(args)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| missing_dependency_message(binary, qemu_package_hint()))?;

    fs::write(runtime_root.join("qemu.pid"), child.id().to_string()).with_context(|| {
        format!(
            "failed to write {}",
            runtime_root.join("qemu.pid").display()
        )
    })?;
    Ok(child.id())
}

pub(super) fn stop_project_qemu_process(runtime_root: &Path) {
    let pid_path = runtime_root.join("qemu.pid");
    let Ok(raw_pid) = fs::read_to_string(&pid_path) else {
        return;
    };
    let Ok(pid) = raw_pid.trim().parse::<u32>() else {
        let _ = fs::remove_file(pid_path);
        return;
    };
    if process_is_running(pid) {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    let _ = fs::remove_file(pid_path);
}

pub(super) fn remove_stale_project_qemu_pid(runtime_root: &Path) {
    let pid_path = runtime_root.join("qemu.pid");
    let Ok(raw_pid) = fs::read_to_string(&pid_path) else {
        return;
    };
    let Ok(pid) = raw_pid.trim().parse::<u32>() else {
        let _ = fs::remove_file(pid_path);
        return;
    };
    if process_is_running(pid) {
        return;
    }
    let _ = fs::remove_file(pid_path);
}

pub(super) fn project_qemu_exit_status(runtime_root: &Path) -> Option<String> {
    let pid_path = runtime_root.join("qemu.pid");
    let raw_pid = fs::read_to_string(&pid_path).ok()?;
    let pid = raw_pid.trim().parse::<u32>().ok()?;
    if process_is_running(pid) {
        None
    } else {
        Some(format!("pid {pid} is no longer running"))
    }
}

pub(super) fn recent_project_qemu_logs(runtime_root: &Path) -> String {
    let mut logs = Vec::new();
    for name in ["qemu.command.txt", "qemu.stderr.log", "qemu.stdout.log"] {
        let path = runtime_root.join(name);
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let trimmed = contents.trim();
        if trimmed.is_empty() {
            continue;
        }
        logs.push(format!("{name}:\n{}", truncate_for_log(trimmed, 4_000)));
    }
    logs.join("\n")
}

pub(super) fn project_qemu_command(runtime_root: &Path) -> Option<String> {
    fs::read_to_string(runtime_root.join("qemu.command.txt"))
        .ok()
        .map(|command| command.trim().to_string())
        .filter(|command| !command.is_empty())
}

pub(super) fn desired_project_qemu_command(
    overlay_image: &Path,
    seed_iso: &Path,
    cpu: u8,
    memory_mb: u32,
    ssh_port: u16,
    network: Option<&VmNetworkLease>,
) -> Result<String> {
    let binary = qemu_binary()?;
    let args = qemu_launch_args(overlay_image, seed_iso, cpu, memory_mb, ssh_port, network);
    Ok(format_qemu_command(binary, &args).trim().to_string())
}

fn format_qemu_command(binary: &str, args: &[String]) -> String {
    format!("{binary} {}\n", shell_join(args))
}

pub(super) fn qemu_launch_args(
    overlay_image: &Path,
    seed_iso: &Path,
    cpu: u8,
    memory_mb: u32,
    ssh_port: u16,
    network: Option<&VmNetworkLease>,
) -> Vec<String> {
    let mut args = vec![
        "-nodefaults".to_string(),
        "-no-user-config".to_string(),
        "-machine".to_string(),
        "q35,usb=off".to_string(),
        "-cpu".to_string(),
        "host".to_string(),
        "-display".to_string(),
        "none".to_string(),
        "-vga".to_string(),
        "none".to_string(),
        "-serial".to_string(),
        "stdio".to_string(),
        "-no-reboot".to_string(),
        "-m".to_string(),
        memory_mb.to_string(),
        "-smp".to_string(),
        cpu.to_string(),
        "-accel".to_string(),
        "kvm".to_string(),
        "-accel".to_string(),
        "tcg,split-wx=on,thread=multi".to_string(),
        "-drive".to_string(),
        format!(
            "if=none,id=rootdisk,format=qcow2,file={}",
            overlay_image.display()
        ),
        "-device".to_string(),
        "virtio-blk-pci,drive=rootdisk,bootindex=0".to_string(),
        "-drive".to_string(),
        format!(
            "if=none,id=seed,format=raw,file={},media=cdrom,readonly=on",
            seed_iso.display()
        ),
        "-device".to_string(),
        "virtio-blk-pci,drive=seed".to_string(),
        "-device".to_string(),
        "virtio-rng-pci".to_string(),
        "-netdev".to_string(),
        format!("user,id=net0,hostfwd=tcp::{}-:22", ssh_port),
        "-device".to_string(),
        format!("virtio-net-pci,netdev=net0,mac={HOSTFWD_NET_MAC_ADDRESS}"),
    ];
    if let Some(network) = network {
        args.extend([
            "-netdev".to_string(),
            format!(
                "tap,id=net1,ifname={},script=no,downscript=no",
                network.tap_name
            ),
            "-device".to_string(),
            format!("virtio-net-pci,netdev=net1,mac={}", network.mac_address),
        ]);
    }
    args
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

                    logs::agent_debug(format!("qemu {label}: {line}"));
                }
                Err(error) => {
                    logs::agent_warn(format!("qemu {label} log read failed: {error}"));
                    break;
                }
            }
        }
    });
}

fn log_qemu_exit_status(result: std::io::Result<std::process::ExitStatus>) {
    match result {
        Ok(status) if status.success() => {
            logs::agent_debug(format!("qemu exited with status: {status}"));
        }
        Ok(status) => logs::agent_warn(format!("qemu exited with status: {status}")),
        Err(error) => logs::agent_warn(format!("failed to wait for qemu: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::qemu_launch_args;

    #[test]
    fn qemu_launch_args_enable_kvm_fallback_and_split_wx_tcg() {
        let overlay_image = Path::new("/tmp/disk.qcow2");
        let seed_iso = Path::new("/tmp/seed.iso");

        let args = qemu_launch_args(overlay_image, seed_iso, 4, 2048, 2222, None);

        assert_eq!(
            args,
            vec![
                "-nodefaults",
                "-no-user-config",
                "-machine",
                "q35,usb=off",
                "-cpu",
                "host",
                "-display",
                "none",
                "-vga",
                "none",
                "-serial",
                "stdio",
                "-no-reboot",
                "-m",
                "2048",
                "-smp",
                "4",
                "-accel",
                "kvm",
                "-accel",
                "tcg,split-wx=on,thread=multi",
                "-drive",
                "if=none,id=rootdisk,format=qcow2,file=/tmp/disk.qcow2",
                "-device",
                "virtio-blk-pci,drive=rootdisk,bootindex=0",
                "-drive",
                "if=none,id=seed,format=raw,file=/tmp/seed.iso,media=cdrom,readonly=on",
                "-device",
                "virtio-blk-pci,drive=seed",
                "-device",
                "virtio-rng-pci",
                "-netdev",
                "user,id=net0,hostfwd=tcp::2222-:22",
                "-device",
                "virtio-net-pci,netdev=net0,mac=52:54:00:12:34:56",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }
}
