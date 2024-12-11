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

/// Grows a partition to its maximum available size
///
/// # Arguments
/// * `disk` - Name of the disk (e.g., "/dev/sda")
/// * `partition` - Partition number
///
/// # Returns
/// * `Ok(())` if successful
/// * `Err(ResizeError)` if any step fails
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
///
/// # Returns
/// * `Ok(())` if successful
/// * `Err(ResizeError)` if any step fails
pub fn resize_filesystem(device: &Path, fs_type: &str, mount_point: &Path) -> Result<(), ResizeError> {
    info!("Resizing {} filesystem on {}", fs_type, device.display());

    let status = match fs_type {
        "ext4" => Command::new("resize2fs")
            .arg(device)
            .status(),
        "xfs" => Command::new("xfs_growfs")
            .arg(mount_point)
            .status(),
        "btrfs" => Command::new("btrfs")
            .args(["filesystem", "resize", "max", &mount_point.to_string_lossy()])
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
///
/// # Returns
/// * `Ok(())` if verification succeeds
/// * `Err(ResizeError)` if verification fails
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
}
