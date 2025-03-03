use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;
use tracing::info;
use which::which;

pub mod resize;

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

/// Checks if all required tools are available in the system
pub fn check_requirements(fs_types: &[&str]) -> Result<(), DeviceError> {
    let mut required_tools = vec!["lsblk", "growpart"];

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
                )))
            }
        }
    }

    for tool in &required_tools {
        which(*tool).map_err(|_| DeviceError::MissingTool((*tool).to_string()))?;
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
    let real_device = std::fs::canonicalize(device_path)
        .map_err(|_| DeviceError::NotFound(device_path.to_path_buf()))?;

    info!("Running lsblk on device: {:?}", real_device);
    let output = Command::new("lsblk")
        .args([
            "-Pno",              // key=value format, no headers
            "pkname,name,partn", // parent kernel name, name, partition number
            real_device.to_str().unwrap(),
        ])
        .output()
        .map_err(|e| DeviceError::DeviceInfo(format!("Failed to execute lsblk: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DeviceError::DeviceInfo(format!(
            "lsblk failed with error: {}",
            stderr
        )));
    }

    let info = String::from_utf8_lossy(&output.stdout);
    if info.trim().is_empty() {
        return Err(DeviceError::DeviceInfo(format!(
            "lsblk returned no output for device {:?}",
            real_device
        )));
    }

    info!("lsblk output: '{}'", info.trim());

    // Parse the key=value format
    let mut disk_name = None;
    let mut partition_number = None;
    let mut name = None;

    for pair in info.trim().split(' ') {
        let mut kv = pair.split('=');
        match (kv.next(), kv.next()) {
            (Some("PKNAME"), Some(pkname)) => {
                let pkname_str = pkname.trim_matches('"');
                if !pkname_str.is_empty() {
                    disk_name = Some(pkname_str.to_string());
                }
            }
            (Some("NAME"), Some(n)) => {
                name = Some(n.trim_matches('"').to_string());
            }
            (Some("PARTN"), Some(num)) => {
                let num_str = num.trim_matches('"');
                if !num_str.is_empty() {
                    partition_number = Some(num_str.parse::<u32>().map_err(|_| {
                        DeviceError::DeviceInfo("Invalid partition number".to_string())
                    })?);
                }
            }
            _ => {}
        }
    }

    let disk_name = disk_name.unwrap_or_else(|| name.clone().unwrap_or_default());

    if disk_name.is_empty() {
        return Err(DeviceError::DeviceInfo(
            "Could not determine device name".to_string(),
        ));
    }

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

    #[test]
    fn test_check_requirements() {
        let fs_types = vec!["ext4", "xfs", "btrfs"];
        match check_requirements(&fs_types) {
            Ok(_) => println!("All tools are available"),
            Err(DeviceError::MissingTool(tool)) => {
                assert!(
                    ["lsblk", "growpart", "resize2fs", "xfs_growfs", "btrfs"]
                        .contains(&tool.as_str()),
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
    fn test_device_info_parsing() {
        let mock_output = "PKNAME=\"sda\" NAME=\"sda1\" PARTN=\"1\"";
        let mut disk_name = None;
        let mut partition_number = None;
        let mut name = None;

        for pair in mock_output.split(' ') {
            let mut kv = pair.split('=');
            match (kv.next(), kv.next()) {
                (Some("PKNAME"), Some(pkname)) => {
                    disk_name = Some(pkname.trim_matches('"').to_string());
                }
                (Some("NAME"), Some(n)) => {
                    name = Some(n.trim_matches('"').to_string());
                }
                (Some("PARTN"), Some(num)) => {
                    let num_str = num.trim_matches('"');
                    partition_number = Some(num_str.parse::<u32>().unwrap());
                }
                _ => {}
            }
        }

        assert_eq!(disk_name.unwrap(), "sda");
        assert_eq!(name.unwrap(), "sda1");
        assert_eq!(partition_number.unwrap(), 1);

        let mock_output = "PKNAME=\"\" NAME=\"vdb\" PARTN=\"\"";
        let mut disk_name = None;
        let mut partition_number = None;
        let mut name = None;

        for pair in mock_output.split(' ') {
            let mut kv = pair.split('=');
            match (kv.next(), kv.next()) {
                (Some("PKNAME"), Some(pkname)) => {
                    let pkname_str = pkname.trim_matches('"');
                    if !pkname_str.is_empty() {
                        disk_name = Some(pkname_str.to_string());
                    }
                }
                (Some("NAME"), Some(n)) => {
                    name = Some(n.trim_matches('"').to_string());
                }
                (Some("PARTN"), Some(num)) => {
                    let num_str = num.trim_matches('"');
                    if !num_str.is_empty() {
                        partition_number = Some(num_str.parse::<u32>().unwrap());
                    }
                }
                _ => {}
            }
        }

        assert_eq!(disk_name, None);
        assert_eq!(name.unwrap(), "vdb");
        assert_eq!(partition_number, None);
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
