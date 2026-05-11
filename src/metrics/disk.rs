use std::path::Path;

use sysinfo::Disks;

#[derive(Debug)]
pub(super) struct DiskMetrics {
    pub used: u64,
    pub total: u64,
}

pub(super) fn collect() -> DiskMetrics {
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
        return DiskMetrics {
            used: total.saturating_sub(available),
            total,
        };
    }

    DiskMetrics { used: 0, total: 1 }
}
