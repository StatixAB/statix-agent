use std::{fs, path::Path};

#[derive(Debug)]
pub(super) struct NetworkMetrics {
    pub rx: u64,
    pub tx: u64,
}

pub(super) fn collect() -> NetworkMetrics {
    #[cfg(target_os = "linux")]
    {
        collect_linux()
    }

    #[cfg(not(target_os = "linux"))]
    {
        warn_unsupported_once();
        NetworkMetrics { rx: 0, tx: 0 }
    }
}

#[cfg(target_os = "linux")]
fn collect_linux() -> NetworkMetrics {
    let entries = match fs::read_dir("/sys/class/net") {
        Ok(entries) => entries,
        Err(_) => return NetworkMetrics { rx: 0, tx: 0 },
    };

    let mut rx = 0u64;
    let mut tx = 0u64;

    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("lo") {
            continue;
        }

        let path = entry.path();

        if fs::read_to_string(path.join("operstate"))
            .map(|state| state.trim() == "down")
            .unwrap_or(false)
        {
            continue;
        }

        rx = rx.saturating_add(read_u64(path.join("statistics/rx_bytes")));
        tx = tx.saturating_add(read_u64(path.join("statistics/tx_bytes")));
    }

    NetworkMetrics { rx, tx }
}

#[cfg(not(target_os = "linux"))]
fn warn_unsupported_once() {
    static HAS_WARNED: std::sync::Once = std::sync::Once::new();

    HAS_WARNED.call_once(|| {
        eprintln!("[Metrics] Network usage collection is not supported on this platform");
    });
}

#[cfg(target_os = "linux")]
fn read_u64(path: impl AsRef<Path>) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse::<u64>().ok())
        .unwrap_or(0)
}
