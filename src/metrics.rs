use std::{collections::HashMap, fs, path::Path};

use anyhow::Result;
use serde::Serialize;
use sysinfo::{Disks, System};

#[derive(Debug, Serialize)]
pub struct MetricsPayload {
    pub v: u8,
    pub ts: u64,
    pub cpu: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_cached: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_available: Option<u64>,
    pub disk_used: u64,
    pub disk_total: u64,
    pub net_rx: u64,
    pub net_tx: u64,
}

#[derive(Debug)]
struct MemorySnapshot {
    total: u64,
    used: u64,
    cached: Option<u64>,
    available: Option<u64>,
}

pub fn collect_metrics() -> Result<MetricsPayload> {
    let cpu_count = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1) as f64;
    let load1m = System::load_average().one;
    let cpu = clamp(load1m / cpu_count, 0.0, 1.0);
    let memory = read_memory_usage();
    let (disk_used, disk_total) = read_disk_usage();

    let (net_rx, net_tx) = read_network_usage();

    Ok(MetricsPayload {
        v: 1,
        ts: unix_timestamp_ms(),
        cpu,
        mem_used: memory.used,
        mem_total: memory.total.max(1),
        mem_cached: memory.cached,
        mem_available: memory.available,
        disk_used,
        disk_total,
        net_rx,
        net_tx,
    })
}

fn clamp(value: f64, min: f64, max: f64) -> f64 {
    value.max(min).min(max)
}

fn read_disk_usage() -> (u64, u64) {
    let disks = Disks::new_with_refreshed_list();
    let root_mount = Path::new("/");
    let selected = disks
        .list()
        .iter()
        .find(|disk| disk.mount_point() == root_mount)
        .or_else(|| disks.list().first());

    if let Some(disk) = selected {
        let total = disk.total_space().max(1);
        let available = disk.available_space().min(total);
        return (total.saturating_sub(available), total);
    }

    (0, 1)
}

fn read_memory_usage() -> MemorySnapshot {
    if let Some(snapshot) = read_cgroup_memory_v2() {
        return snapshot;
    }

    if let Some(snapshot) = read_proc_meminfo() {
        return snapshot;
    }

    let mut system = System::new();
    system.refresh_memory();
    let total = system.total_memory().max(1);
    let available = system.available_memory().min(total);
    MemorySnapshot {
        total,
        used: total.saturating_sub(available),
        cached: None,
        available: Some(available),
    }
}

fn read_cgroup_memory_v2() -> Option<MemorySnapshot> {
    let max_raw = fs::read_to_string("/sys/fs/cgroup/memory.max").ok()?;
    let current_raw = fs::read_to_string("/sys/fs/cgroup/memory.current").ok()?;
    let stat_raw = fs::read_to_string("/sys/fs/cgroup/memory.stat").ok()?;

    let max_trimmed = max_raw.trim();
    if max_trimmed == "max" {
        return None;
    }

    let total = parse_positive_number(max_trimmed)?;
    let current = parse_positive_number(current_raw.trim())?;
    if total == 0 {
        return None;
    }

    let cached = stat_raw.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("file"), Some(value)) => parse_positive_number(value),
            _ => None,
        }
    });

    let used = current.min(total);
    Some(MemorySnapshot {
        total,
        used,
        cached: cached.map(|value| value.min(used)),
        available: Some(total.saturating_sub(used)),
    })
}

fn read_proc_meminfo() -> Option<MemorySnapshot> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;
    let values = parse_meminfo(&content);

    let total = read_meminfo_value_bytes(&values, "MemTotal")?;
    let available = read_meminfo_value_bytes(&values, "MemAvailable")?;
    let buffers = read_meminfo_value_bytes(&values, "Buffers").unwrap_or(0);
    let cached = read_meminfo_value_bytes(&values, "Cached").unwrap_or(0);
    let reclaimable = read_meminfo_value_bytes(&values, "SReclaimable").unwrap_or(0);
    let shmem = read_meminfo_value_bytes(&values, "Shmem").unwrap_or(0);
    let htop_style_cache = buffers
        .saturating_add(cached)
        .saturating_add(reclaimable)
        .saturating_sub(shmem);
    let used = total.saturating_sub(available).min(total);

    Some(MemorySnapshot {
        total,
        used,
        cached: Some(htop_style_cache.min(used)),
        available: Some(total.saturating_sub(used)),
    })
}

fn read_network_usage() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        let entries = match fs::read_dir("/sys/class/net") {
            Ok(entries) => entries,
            Err(_) => return (0, 0),
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

        return (rx, tx);

        #[cfg(not(target_os = "linux"))]
        {
            warm_unsupported_network_metrics_once();
            (0, 0)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn warm_unsupported_network_metrics_once() {
    static HAS_WARNED: std::sync::Once = std::sync::Once::new();

    HAS_WARNED.call_once(|| {
        eprintln!("[Metrics] Network usage collection is not supported on this platform");
    });
}

fn read_u64(path: impl AsRef<Path>) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn parse_meminfo(content: &str) -> HashMap<String, u64> {
    content
        .lines()
        .filter_map(|line| {
            let (key, raw_value) = line.split_once(':')?;
            let numeric = raw_value.split_whitespace().next()?.parse::<u64>().ok()?;
            Some((key.trim().to_owned(), numeric))
        })
        .collect()
}

fn read_meminfo_value_bytes(values: &HashMap<String, u64>, key: &str) -> Option<u64> {
    values
        .get(key)
        .copied()
        .map(|value| value.saturating_mul(1024))
}

fn parse_positive_number(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()
}

fn unix_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
