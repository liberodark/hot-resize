use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::debug;

pub mod resize;

/// Searches for an executable in the system PATH.
///
/// Returns the full path if found, or None if not found.
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let full_path = dir.join(name);
            if full_path.is_file() {
                Some(full_path)
            } else {
                None
            }
        })
    })
}

#[derive(Debug)]
pub struct BlockDevice {
    pub real_device: PathBuf,
    pub disk_name: String,
    pub partition_number: Option<u32>,
}

#[derive(Error, Debug)]
pub enum DeviceError {
    #[error("Device not found: {0}")]
    NotFound(PathBuf),
    #[error("Required tool not found: {0}")]
    MissingTool(String),
    #[error("Failed to get device info: {0}")]
    DeviceInfo(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Returns the size of a block device in bytes by reading sysfs.
///
/// Reads `/sys/class/block/<device_name>/size` which contains the number
/// of 512-byte sectors, then converts to bytes.
///
/// # Arguments
/// * `device_path` - Path to the block device (e.g. `/dev/sda1`)
pub fn get_device_size(device_path: &Path) -> Result<u64, DeviceError> {
    let dev_name = resolve_device_name(device_path)?;
    let sysfs_base = Path::new("/sys/class/block");
    read_sysfs_device_size(sysfs_base, &dev_name)
}

/// Reads the size of a block device from a sysfs-like directory structure.
///
/// Expects a file at `<sysfs_base>/<dev_name>/size` containing the sector count.
/// The sector size in sysfs is always 512 bytes.
fn read_sysfs_device_size(sysfs_base: &Path, dev_name: &str) -> Result<u64, DeviceError> {
    let size_path = sysfs_base.join(dev_name).join("size");

    let sectors_str = std::fs::read_to_string(&size_path).map_err(|e| {
        DeviceError::DeviceInfo(format!("Failed to read sysfs size for {}: {}", dev_name, e))
    })?;

    let sectors: u64 = sectors_str.trim().parse().map_err(|e| {
        DeviceError::DeviceInfo(format!(
            "Failed to parse sector count for {}: {}",
            dev_name, e
        ))
    })?;

    // sysfs always reports in 512-byte sectors
    Ok(sectors * 512)
}

/// Resolves a device path to its kernel name (e.g. `/dev/sda1` → `sda1`, `/dev/mapper/foo` → `dm-0`)
fn resolve_device_name(device_path: &Path) -> Result<String, DeviceError> {
    let real_path = std::fs::canonicalize(device_path)
        .map_err(|_| DeviceError::NotFound(device_path.to_path_buf()))?;

    real_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(String::from)
        .ok_or_else(|| {
            DeviceError::DeviceInfo(format!(
                "Could not extract device name from {:?}",
                real_path
            ))
        })
}

/// Checks if all required tools are available in the system
pub fn check_requirements(fs_types: &[&str]) -> Result<(), DeviceError> {
    let mut required_tools = vec!["sfdisk"];

    // Add filesystem-specific tools based on the provided types
    for fs_type in fs_types {
        match *fs_type {
            "ext4" | "ext3" | "ext2" => {
                if !required_tools.contains(&"resize2fs") {
                    required_tools.push("resize2fs");
                }
            }
            "xfs" => {
                if !required_tools.contains(&"xfs_growfs") {
                    required_tools.push("xfs_growfs");
                }
            }
            "btrfs" => {
                if !required_tools.contains(&"btrfs") {
                    required_tools.push("btrfs");
                }
            }
            _ => {
                return Err(DeviceError::MissingTool(format!(
                    "Unsupported filesystem: {}",
                    fs_type
                )));
            }
        }
    }

    for tool in &required_tools {
        if find_in_path(tool).is_none() {
            return Err(DeviceError::MissingTool((*tool).to_string()));
        }
    }

    Ok(())
}

/// Analyzes a device and returns its information using sysfs.
///
/// Reads `/sys/class/block/<device_name>/partition` for partition number
/// and resolves the parent disk via symlink traversal.
///
/// # Arguments
/// * `device_path` - Path to the device to analyze
///
/// # Returns
/// * `BlockDevice` containing device information
/// * `DeviceError` if analysis fails
pub fn analyze_device(device_path: &Path) -> Result<BlockDevice, DeviceError> {
    let real_device = std::fs::canonicalize(device_path)
        .map_err(|_| DeviceError::NotFound(device_path.to_path_buf()))?;

    let dev_name = real_device
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            DeviceError::DeviceInfo(format!(
                "Could not extract device name from {:?}",
                real_device
            ))
        })?;

    let sysfs_base = Path::new("/sys/class/block");
    let (disk_name, partition_number) = analyze_device_sysfs(sysfs_base, dev_name)?;

    debug!(
        "Device {:?}: disk={}, partition={:?}",
        real_device, disk_name, partition_number
    );

    Ok(BlockDevice {
        real_device,
        disk_name,
        partition_number,
    })
}

/// Reads device info from a sysfs-like directory structure.
///
/// For a partition (e.g. `sda1`):
/// - `/sys/class/block/sda1/partition` contains the partition number
/// - The symlink `/sys/class/block/sda1` points to `../../devices/.../sda/sda1`,
///   so the parent directory name is the disk name
///
/// For a whole disk (e.g. `vdb`):
/// - `/sys/class/block/vdb/partition` does not exist
/// - The disk name is the device name itself
fn analyze_device_sysfs(
    sysfs_base: &Path,
    dev_name: &str,
) -> Result<(String, Option<u32>), DeviceError> {
    let sys_path = sysfs_base.join(dev_name);

    if !sys_path.exists() {
        return Err(DeviceError::DeviceInfo(format!(
            "sysfs entry not found for device {}",
            dev_name
        )));
    }

    // Check if it's a partition
    let partition_file = sys_path.join("partition");
    let partition_number = if partition_file.exists() {
        let num_str = std::fs::read_to_string(&partition_file).map_err(|e| {
            DeviceError::DeviceInfo(format!(
                "Failed to read partition number for {}: {}",
                dev_name, e
            ))
        })?;
        Some(num_str.trim().parse::<u32>().map_err(|_| {
            DeviceError::DeviceInfo(format!("Invalid partition number for {}", dev_name))
        })?)
    } else {
        None
    };

    // Find parent disk name
    let disk_name = if partition_number.is_some() {
        // Follow symlink: /sys/class/block/sda1 -> ../../devices/.../sda/sda1
        // Parent of the link target gives us the disk name
        let link_target = std::fs::read_link(&sys_path).map_err(|e| {
            DeviceError::DeviceInfo(format!("Failed to read symlink for {}: {}", dev_name, e))
        })?;
        link_target
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(String::from)
            .unwrap_or_else(|| dev_name.to_string())
    } else {
        dev_name.to_string()
    };

    if disk_name.is_empty() {
        return Err(DeviceError::DeviceInfo(
            "Could not determine device name".to_string(),
        ));
    }

    Ok((disk_name, partition_number))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Creates a fake sysfs block device directory and returns the base path.
    /// Structure: <tmpdir>/sysfs/class/block/<dev_name>/size
    fn create_fake_sysfs(dev_name: &str, sector_count: &str) -> (tempfile::TempDir, PathBuf) {
        let tmpdir = tempfile::tempdir().expect("Failed to create temp dir");
        let dev_dir = tmpdir.path().join(dev_name);
        std::fs::create_dir_all(&dev_dir).expect("Failed to create device dir");
        std::fs::write(dev_dir.join("size"), sector_count).expect("Failed to write size file");
        let base = tmpdir.path().to_path_buf();
        (tmpdir, base)
    }

    #[test]
    fn test_read_sysfs_size_basic() {
        // 2048 sectors * 512 bytes = 1 MiB
        let (_tmpdir, base) = create_fake_sysfs("sda1", "2048\n");
        let size = read_sysfs_device_size(&base, "sda1").unwrap();
        assert_eq!(size, 2048 * 512);
    }

    #[test]
    fn test_read_sysfs_size_large_disk() {
        // Simulate a 1 TiB disk: 2147483648 sectors * 512 = 1 TiB
        let (_tmpdir, base) = create_fake_sysfs("vda", "2147483648\n");
        let size = read_sysfs_device_size(&base, "vda").unwrap();
        assert_eq!(size, 1_099_511_627_776); // 1 TiB in bytes
    }

    #[test]
    fn test_read_sysfs_size_no_trailing_newline() {
        let (_tmpdir, base) = create_fake_sysfs("sdb", "1024");
        let size = read_sysfs_device_size(&base, "sdb").unwrap();
        assert_eq!(size, 1024 * 512);
    }

    #[test]
    fn test_read_sysfs_size_with_extra_whitespace() {
        let (_tmpdir, base) = create_fake_sysfs("nvme0n1", "  4096  \n");
        let size = read_sysfs_device_size(&base, "nvme0n1").unwrap();
        assert_eq!(size, 4096 * 512);
    }

    #[test]
    fn test_read_sysfs_size_zero() {
        let (_tmpdir, base) = create_fake_sysfs("loop0", "0\n");
        let size = read_sysfs_device_size(&base, "loop0").unwrap();
        assert_eq!(size, 0);
    }

    #[test]
    fn test_read_sysfs_size_device_not_found() {
        let tmpdir = tempfile::tempdir().expect("Failed to create temp dir");
        let result = read_sysfs_device_size(tmpdir.path(), "nonexistent");
        assert!(matches!(result, Err(DeviceError::DeviceInfo(_))));
    }

    #[test]
    fn test_read_sysfs_size_invalid_content() {
        let (_tmpdir, base) = create_fake_sysfs("bad_dev", "not_a_number\n");
        let result = read_sysfs_device_size(&base, "bad_dev");
        assert!(matches!(result, Err(DeviceError::DeviceInfo(_))));
    }

    #[test]
    fn test_read_sysfs_size_empty_file() {
        let (_tmpdir, base) = create_fake_sysfs("empty_dev", "");
        let result = read_sysfs_device_size(&base, "empty_dev");
        assert!(matches!(result, Err(DeviceError::DeviceInfo(_))));
    }

    #[test]
    fn test_resolve_device_name_nonexistent() {
        let result = resolve_device_name(Path::new("/dev/nonexistent_xyz"));
        assert!(matches!(result, Err(DeviceError::NotFound(_))));
    }

    #[test]
    fn test_resolve_device_name_regular_file() {
        // Use a real file to test the name extraction logic
        let tmpdir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = tmpdir.path().join("test_device");
        std::fs::write(&file_path, "").expect("Failed to create test file");

        let name = resolve_device_name(&file_path).unwrap();
        assert_eq!(name, "test_device");
    }

    #[test]
    fn test_resolve_device_name_symlink() {
        let tmpdir = tempfile::tempdir().expect("Failed to create temp dir");
        let real_file = tmpdir.path().join("real_name");
        let link_path = tmpdir.path().join("link_name");
        std::fs::write(&real_file, "").expect("Failed to create real file");
        std::os::unix::fs::symlink(&real_file, &link_path).expect("Failed to create symlink");

        // Should resolve through the symlink to the real name
        let name = resolve_device_name(&link_path).unwrap();
        assert_eq!(name, "real_name");
    }

    /// Creates a fake sysfs partition entry.
    /// Simulates: /sys/class/block/<part_name> -> <disk_name>/<part_name>/
    /// with a `partition` file containing the partition number.
    fn create_fake_sysfs_partition(base: &Path, disk_name: &str, part_name: &str, part_num: u32) {
        // Create the real directory: <base>/<disk_name>/<part_name>/
        let real_dir = base.join(disk_name).join(part_name);
        std::fs::create_dir_all(&real_dir).expect("Failed to create partition dir");
        std::fs::write(real_dir.join("partition"), format!("{}\n", part_num))
            .expect("Failed to write partition file");

        // Create symlink: <base>/<part_name> -> <disk_name>/<part_name>
        let link_path = base.join(part_name);
        let target = PathBuf::from(disk_name).join(part_name);
        std::os::unix::fs::symlink(&target, &link_path).expect("Failed to create symlink");
    }

    /// Creates a fake sysfs whole disk entry (directory, no partition file).
    fn create_fake_sysfs_disk(base: &Path, disk_name: &str) {
        let disk_dir = base.join(disk_name);
        std::fs::create_dir_all(&disk_dir).expect("Failed to create disk dir");
    }

    #[test]
    fn test_analyze_sysfs_partition() {
        let tmpdir = tempfile::tempdir().expect("Failed to create temp dir");
        create_fake_sysfs_partition(tmpdir.path(), "sda", "sda1", 1);

        let (disk_name, part_num) = analyze_device_sysfs(tmpdir.path(), "sda1").unwrap();
        assert_eq!(disk_name, "sda");
        assert_eq!(part_num, Some(1));
    }

    #[test]
    fn test_analyze_sysfs_partition_high_number() {
        let tmpdir = tempfile::tempdir().expect("Failed to create temp dir");
        create_fake_sysfs_partition(tmpdir.path(), "nvme0n1", "nvme0n1p15", 15);

        let (disk_name, part_num) = analyze_device_sysfs(tmpdir.path(), "nvme0n1p15").unwrap();
        assert_eq!(disk_name, "nvme0n1");
        assert_eq!(part_num, Some(15));
    }

    #[test]
    fn test_analyze_sysfs_whole_disk() {
        let tmpdir = tempfile::tempdir().expect("Failed to create temp dir");
        create_fake_sysfs_disk(tmpdir.path(), "vdb");

        let (disk_name, part_num) = analyze_device_sysfs(tmpdir.path(), "vdb").unwrap();
        assert_eq!(disk_name, "vdb");
        assert_eq!(part_num, None);
    }

    #[test]
    fn test_analyze_sysfs_device_not_in_sysfs() {
        let tmpdir = tempfile::tempdir().expect("Failed to create temp dir");
        let result = analyze_device_sysfs(tmpdir.path(), "nonexistent");
        assert!(matches!(result, Err(DeviceError::DeviceInfo(_))));
    }

    #[test]
    fn test_analyze_sysfs_invalid_partition_number() {
        let tmpdir = tempfile::tempdir().expect("Failed to create temp dir");
        let dev_dir = tmpdir.path().join("bad_dev");
        std::fs::create_dir_all(&dev_dir).expect("Failed to create dir");
        std::fs::write(dev_dir.join("partition"), "not_a_number\n")
            .expect("Failed to write partition file");

        let result = analyze_device_sysfs(tmpdir.path(), "bad_dev");
        assert!(matches!(result, Err(DeviceError::DeviceInfo(_))));
    }

    #[test]
    fn test_find_in_path_existing_tool() {
        // "sh" should exist on any Unix system
        assert!(find_in_path("sh").is_some());
    }

    #[test]
    fn test_find_in_path_nonexistent_tool() {
        assert!(find_in_path("nonexistent_tool_xyz_12345").is_none());
    }

    #[test]
    fn test_find_in_path_returns_full_path() {
        if let Some(path) = find_in_path("sh") {
            assert!(path.is_absolute() || path.starts_with("/"));
            assert!(path.is_file());
        }
    }

    #[test]
    fn test_check_requirements() {
        let fs_types = vec!["ext4", "xfs", "btrfs"];
        match check_requirements(&fs_types) {
            Ok(_) => println!("All tools are available"),
            Err(DeviceError::MissingTool(tool)) => {
                assert!(
                    ["sfdisk", "resize2fs", "xfs_growfs", "btrfs"].contains(&tool.as_str()),
                    "Unexpected missing tool: {}",
                    tool
                );
            }
            Err(e) => panic!("Unexpected error type: {}", e),
        }
    }

    #[test]
    fn test_analyze_invalid_device() {
        let result = analyze_device(Path::new("/dev/nonexistent"));
        assert!(matches!(result, Err(DeviceError::NotFound(_))));
    }

    #[test]
    fn test_block_device_creation() {
        let device = BlockDevice {
            real_device: PathBuf::from("/dev/sda1"),
            disk_name: "sda".to_string(),
            partition_number: Some(1),
        };

        assert_eq!(device.disk_name, "sda");
        assert_eq!(device.partition_number, Some(1));
        assert_eq!(device.real_device, PathBuf::from("/dev/sda1"));

        let device = BlockDevice {
            real_device: PathBuf::from("/dev/vdb"),
            disk_name: "vdb".to_string(),
            partition_number: None,
        };

        assert_eq!(device.disk_name, "vdb");
        assert_eq!(device.partition_number, None);
        assert_eq!(device.real_device, PathBuf::from("/dev/vdb"));
    }
}
