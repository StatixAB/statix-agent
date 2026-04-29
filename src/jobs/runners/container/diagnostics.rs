use std::{fs, path::Path};

pub(super) fn summarize_raw_command_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "no output".to_string(),
        (false, true) => format!("stdout:\n{stdout}"),
        (true, false) => format!("stderr:\n{stderr}"),
        (false, false) => format!("stdout:\n{stdout}\n\nstderr:\n{stderr}"),
    }
}

pub(super) fn lxc_log_excerpt(path: &Path) -> String {
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

pub(super) fn lxc_start_failure_message(log_excerpt: &str) -> String {
    if log_excerpt.contains("lxc-user-nic failed to configure requested network")
        && log_excerpt.contains("Read-only file system")
        && log_excerpt.contains("/run/lxc/nics")
    {
        return format!(
            "{log_excerpt}\nHint: lxc-user-nic needs write access to /run/lxc/nics to serialize unprivileged veth allocation. If statix-agent is running under systemd with ProtectSystem=strict, add ReadWritePaths=/run/lxc and restart the service."
        );
    }

    lxc_start_failure_message_with_host_context(log_excerpt, proc_mount_uses_noatime())
}

fn lxc_start_failure_message_with_host_context(
    log_excerpt: &str,
    proc_uses_noatime: Option<bool>,
) -> String {
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
                " If ReadWritePaths is already applied, check whether systemd ProtectKernelTunables is enabled for statix-agent; it can make proc/sys paths read-only in the service mount namespace and block LXC's procfs mount. Also check the host /proc mount options with `findmnt -no OPTIONS /proc`; unprivileged LXC can fail when /proc is mounted with noatime instead of relatime.",
            ),
            none => message.push_str(
                " If ReadWritePaths is already applied, check whether systemd ProtectKernelTunables is enabled for statix-agent; it can make proc/sys paths read-only in the service mount namespace and block LXC's procfs mount. Also check the host /proc mount options; unprivileged LXC can fail when /proc is mounted with noatime instead of relatime.",
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

pub(super) fn missing_dependency_message(program: &str, debian_package: &str) -> String {
    format!(
        "failed to launch {program}; install the '{debian_package}' package and ensure {program} is on PATH"
    )
}
