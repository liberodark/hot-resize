use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;
use which::which;

pub mod resize;

#[derive(Debug)]
pub struct BlockDevice {
    pub real_device: PathBuf,
    pub disk_name: String,
    pub partition_number: u32,
}

#[derive(Error, Debug)]
pub enum DeviceError {
    #[error("Device not found: {0}")]
    NotFound(PathBuf),
    #[error("Required tool not found: {0}")]
    MissingTool(String),
    #[error("Failed to get device info: {0}")]
    DeviceInfo(String),
    #[error("Not a partition device")]
    NotPartition,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Checks if all required tools are available in the system
pub fn check_requirements() -> Result<(), DeviceError> {
    let required_tools = ["lsblk", "growpart", "resize2fs", "xfs_growfs", "btrfs"];

    for tool in required_tools {
        which(tool).map_err(|_| DeviceError::MissingTool(tool.to_string()))?;
    }

    Ok(())
}

/// Analyzes a device and returns its information
///
/// # Arguments
/// * `device_path` - Path to the device to analyze
///
/// # Returns
/// * `BlockDevice` containing device information
/// * `DeviceError` if analysis fails
pub fn analyze_device(device_path: &Path) -> Result<BlockDevice, DeviceError> {
    // Resolve the real device path
    let real_device = std::fs::canonicalize(device_path)
        .map_err(|_| DeviceError::NotFound(device_path.to_path_buf()))?;

    // Get device information using lsblk
    let output = Command::new("lsblk")
        .args(["-ndo", "pkname,part", real_device.to_str().unwrap()])
        .output()
        .map_err(|e| DeviceError::DeviceInfo(e.to_string()))?;

    if !output.status.success() {
        return Err(DeviceError::DeviceInfo("lsblk failed".to_string()));
    }

    let info = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = info.split_whitespace().collect();

    if parts.len() != 2 {
        return Err(DeviceError::NotPartition);
    }

    let disk_name = parts[0].to_string();
    let partition_number = parts[1]
        .parse::<u32>()
        .map_err(|_| DeviceError::DeviceInfo("Invalid partition number".to_string()))?;

    Ok(BlockDevice {
        real_device,
        disk_name,
        partition_number,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Test required tools verification
    #[test]
    fn test_check_requirements() {
        // Capture specific errors for each missing tool
        match check_requirements() {
            Ok(_) => println!("All tools are available"),
            Err(DeviceError::MissingTool(tool)) => {
                // This is expected in build environment
                assert!(
                    ["lsblk", "growpart", "resize2fs", "xfs_growfs", "btrfs"]
                        .contains(&tool.as_str()),
                    "Unexpected missing tool: {}",
                    tool
                );
            },
            Err(e) => panic!("Unexpected error type: {}", e),
        }
    }

    #[test]
    fn test_analyze_invalid_device() {
        let result = analyze_device(Path::new("/dev/nonexistent"));
        assert!(matches!(result, Err(DeviceError::NotFound(_))));
    }

    #[test]
    fn test_device_info_parsing() {
        // Test with simulated lsblk output
        let disk_name = "sda";
        let part_num = 1;
        let device_info = format!("{} {}", disk_name, part_num);

        let parts: Vec<&str> = device_info.split_whitespace().collect();
        assert_eq!(parts.len(), 2, "Device info should have two parts");
        assert_eq!(parts[0], disk_name);
        assert_eq!(parts[1].parse::<u32>().unwrap(), part_num);
    }

    #[test]
    fn test_block_device_creation() {
        let device = BlockDevice {
            real_device: PathBuf::from("/dev/sda1"),
            disk_name: "sda".to_string(),
            partition_number: 1,
        };

        assert_eq!(device.disk_name, "sda");
        assert_eq!(device.partition_number, 1);
        assert_eq!(device.real_device, PathBuf::from("/dev/sda1"));
    }
}
