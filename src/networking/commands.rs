use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

pub(super) fn link_exists(name: &str) -> bool {
    Command::new("ip")
        .args(["link", "show", "dev", name])
        .output()
        .is_ok_and(|output| output.status.success())
}

pub(super) fn delete_link_best_effort(name: &str) {
    let _ = Command::new("ip").args(["link", "delete", name]).output();
}

pub(super) fn run_ip(args: &[&str]) -> Result<()> {
    let output = Command::new("ip")
        .args(args)
        .output()
        .context("failed to launch ip command")?;
    if !output.status.success() {
        bail!("{}", command_failure("ip", args, &output));
    }
    Ok(())
}

pub(super) fn command_output(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to launch {program}"))?;
    if !output.status.success() {
        bail!("{}", command_failure(program, args, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn command_failure(program: &str, args: &[&str], output: &Output) -> String {
    let command = format!("{} {}", program, args.join(" "));
    let detail = command_detail(output);

    if program == "ip" && detail.contains("Operation not permitted") {
        return format!(
            "network setup requires CAP_NET_ADMIN or root privileges; `{command}` failed: {detail}"
        );
    }

    if program == "ip"
        && (detail.contains("does not exist") || detail.contains("Cannot find device"))
    {
        return format!("`{command}` failed because the network device was not found: {detail}");
    }

    if detail.is_empty() {
        format!("`{command}` failed with {}", output.status)
    } else {
        format!("`{command}` failed with {}: {detail}", output.status)
    }
}

fn command_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
