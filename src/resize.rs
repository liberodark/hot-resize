use std::path::Path;
use std::process::Command;
use thiserror::Error;
use tracing::{info, warn};

#[derive(Error, Debug)]
pub enum ResizeError {
    #[error("Failed to grow partition: {0}")]
    GrowPartition(String),
    #[error("Failed to resize filesystem: {0}")]
    ResizeFs(String),
    #[error("Command execution failed: {0}")]
    CommandFailed(String),
    #[error("Device not found: {0}")]
    DeviceNotFound(String),
}

/// Gets the filesystem type of a device
pub fn get_fs_type(device: &Path) -> Result<String, ResizeError> {
    let output = Command::new("blkid")
        .arg("-s")
        .arg("TYPE")
        .arg("-o")
        .arg("value")
        .arg(device)
        .output()
        .map_err(|e| ResizeError::CommandFailed(e.to_string()))?;

    if !output.status.success() {
        return Err(ResizeError::ResizeFs("Failed to detect filesystem type".to_string()));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Grows a partition to its maximum available size
///
/// # Arguments
/// * `disk` - Name of the disk (e.g., "/dev/sda")
/// * `partition` - Partition number
pub fn grow_partition(disk: &str, partition: u32) -> Result<(), ResizeError> {
    info!("Growing partition {} on disk {}", partition, disk);

    let output = Command::new("growpart")
        .args([disk, &partition.to_string()])
        .output()
        .map_err(|e| ResizeError::CommandFailed(e.to_string()))?;

    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        // growpart returns 2 when partition is already at maximum size
        if output.status.code() == Some(2) {
            warn!("Partition is already at maximum size");
            return Ok(());
        }
        return Err(ResizeError::GrowPartition(error.to_string()));
    }

    info!("Successfully grew partition");
    Ok(())
}

/// Resizes a filesystem to its maximum available size
///
/// # Arguments
/// * `device` - Path to the device (e.g., "/dev/sda1")
/// * `fs_type` - Filesystem type ("ext4", "xfs", or "btrfs")
/// * `mount_point` - Mount point of the filesystem
pub fn resize_filesystem(device: &Path, fs_type: &str, mount_point: &Path) -> Result<(), ResizeError> {
    // Verify actual filesystem type
    let real_fs_type = get_fs_type(device)?;
    if real_fs_type != fs_type {
        info!("Detected filesystem type {} differs from specified {}", real_fs_type, fs_type);
        // Use real type
        return resize_fs(&real_fs_type, device, mount_point);
    }

    resize_fs(fs_type, device, mount_point)
}

/// Internal function to perform the actual filesystem resize
fn resize_fs(fs_type: &str, device: &Path, mount_point: &Path) -> Result<(), ResizeError> {
    info!("Resizing {} filesystem on {}", fs_type, device.display());

    let status = match fs_type.to_lowercase().as_str() {
        "ext4" => {
            Command::new("resize2fs")
                .arg("-f")
                .arg(device)
                .status()
        },
        "xfs" => Command::new("xfs_growfs")
            .arg(mount_point)
            .status(),
        "btrfs" => Command::new("btrfs")
            .args([
                "filesystem",
                "resize",
                "max",
                &mount_point.to_string_lossy()
            ])
            .status(),
        _ => return Err(ResizeError::ResizeFs(format!("Unsupported filesystem: {}", fs_type))),
    }.map_err(|e| ResizeError::CommandFailed(e.to_string()))?;

    if !status.success() {
        return Err(ResizeError::ResizeFs(format!(
            "Failed to resize {} filesystem", fs_type
        )));
    }

    info!("Successfully resized filesystem");
    Ok(())
}

/// Verifies filesystem size after resize
///
/// # Arguments
/// * `mount_point` - Mount point to verify
pub fn verify_resize(mount_point: &Path) -> Result<(), ResizeError> {
    info!("Verifying resize at {}", mount_point.display());

    let output = Command::new("df")
        .args(["-h", &mount_point.to_string_lossy()])
        .output()
        .map_err(|e| ResizeError::CommandFailed(e.to_string()))?;

    if !output.status.success() {
        return Err(ResizeError::CommandFailed("Failed to get filesystem size".into()));
    }

    info!("Current size:\n{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_resize() {
        let root_path = Path::new("/");
        let result = verify_resize(root_path);
        assert!(result.is_ok(), "Should be able to verify root filesystem");
    }

    #[test]
    fn test_grow_partition_invalid() {
        let result = grow_partition("/dev/nonexistent", 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_resize_filesystem_invalid() {
        let result = resize_filesystem(
            Path::new("/dev/nonexistent"),
            "ext4",
            Path::new("/nonexistent")
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_get_fs_type_invalid() {
        let result = get_fs_type(Path::new("/dev/nonexistent"));
        assert!(result.is_err());
    }
}
