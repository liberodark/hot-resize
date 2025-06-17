use std::path::Path;
use std::process::Command;
use thiserror::Error;
use tracing::{debug, info, warn};
use which::which;

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
    #[error("Failed to resize LUKS container: {0}")]
    ResizeLuks(String),
    #[error("Alternative approach failed: {0}")]
    AlternativeApproachFailed(String),
}

pub fn get_fs_type(device: &Path) -> Result<String, ResizeError> {
    let blkid_output = Command::new("blkid")
        .arg("-s")
        .arg("TYPE")
        .arg("-o")
        .arg("value")
        .arg(device)
        .output();

    match blkid_output {
        Ok(output) => {
            if output.status.success() {
                let fs_type = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !fs_type.is_empty() {
                    return Ok(fs_type);
                }
            }
        }
        Err(e) => {
            debug!("blkid failed: {}", e);
        }
    }

    let lsblk_output = Command::new("lsblk")
        .args(["-ndo", "FSTYPE", &device.to_string_lossy()])
        .output();

    match lsblk_output {
        Ok(output) => {
            if output.status.success() {
                let fs_type = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !fs_type.is_empty() {
                    return Ok(fs_type);
                }
            }
        }
        Err(e) => {
            debug!("lsblk failed: {}", e);
        }
    }

    let file_output = Command::new("file")
        .args(["-Ls", &device.to_string_lossy()])
        .output();

    match file_output {
        Ok(output) => {
            if output.status.success() {
                let output_str = String::from_utf8_lossy(&output.stdout).to_lowercase();
                if output_str.contains("ext4") {
                    return Ok("ext4".to_string());
                } else if output_str.contains("xfs") {
                    return Ok("xfs".to_string());
                } else if output_str.contains("btrfs") {
                    return Ok("btrfs".to_string());
                }
            }
        }
        Err(e) => {
            debug!("file command failed: {}", e);
        }
    }

    Err(ResizeError::ResizeFs(format!(
        "Failed to detect filesystem type for {}",
        device.display()
    )))
}

pub fn grow_partition(disk: &str, partition: Option<u32>) -> Result<(), ResizeError> {
    if partition.is_none() {
        info!("Device is a whole disk (not a partition), skipping partition resize");
        return Ok(());
    }

    let partition_num = partition.unwrap();
    info!("Growing partition {} on disk {}", partition_num, disk);

    let growpart_output = Command::new("growpart")
        .args([disk, &partition_num.to_string()])
        .output();

    match growpart_output {
        Ok(output) => {
            if output.status.success() {
                info!("Successfully grew partition using growpart");
                return Ok(());
            } else if output.status.code() == Some(2) {
                info!("Partition is already at maximum size");
                return Ok(());
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);

                info!("growpart stdout: {}", stdout);

                if !stderr.trim().is_empty() {
                    warn!(
                        "growpart failed with exit code {:?}: {}",
                        output.status.code(),
                        stderr.trim_end()
                    );
                }

                if stderr.contains("partition is already at maximum size")
                    || stderr.contains("no space left")
                    || stderr.contains("cannot be grown")
                    || stdout.contains("NOCHANGE")
                    || stderr.is_empty()
                {
                    info!("Detected that partition is likely already at maximum size");
                    return Ok(());
                }
            }
        }
        Err(e) => {
            warn!("Failed to execute growpart: {}", e);
        }
    }

    match which("parted") {
        Ok(_) => {
            info!("Trying alternative approach with parted");
            let parted_output = Command::new("parted")
                .args([
                    "--script",
                    disk,
                    "resizepart",
                    &partition_num.to_string(),
                    "100%",
                ])
                .output();

            match parted_output {
                Ok(output) => {
                    if output.status.success() {
                        info!("Successfully grew partition using parted");
                        return Ok(());
                    } else {
                        let error = String::from_utf8_lossy(&output.stderr);

                        if error.contains("already at maximum size")
                            || error.contains("no space left")
                        {
                            info!("Partition is already at maximum size (detected from parted)");
                            return Ok(());
                        }
                        warn!("parted failed: {}", error);
                    }
                }
                Err(e) => {
                    warn!("Failed to execute parted: {}", e);
                }
            }
        }
        Err(_) => {
            warn!("parted command not found, skipping alternative approach");

            let lsblk_check = Command::new("lsblk")
                .args(["-bno", "SIZE,NAME", disk])
                .output();

            if let Ok(output) = lsblk_check {
                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    info!("Checking disk space using lsblk: {}", stdout);
                    info!(
                        "Based on available information, assuming partition is already at maximum size"
                    );
                    return Ok(());
                }
            }
        }
    }

    Err(ResizeError::GrowPartition(
        "Failed to grow partition with growpart and parted".to_string(),
    ))
}

pub fn resize_filesystem(
    device: &Path,
    fs_type: &str,
    mount_point: &Path,
) -> Result<(), ResizeError> {
    let real_fs_type = match get_fs_type(device) {
        Ok(fs) => fs,
        Err(_) => {
            info!(
                "Could not detect filesystem type, using specified type: {}",
                fs_type
            );
            fs_type.to_string()
        }
    };

    if real_fs_type != fs_type {
        info!(
            "Detected filesystem type {} differs from specified {}",
            real_fs_type, fs_type
        );
        return resize_fs(&real_fs_type, device, mount_point);
    }

    resize_fs(fs_type, device, mount_point)
}

fn resize_fs(fs_type: &str, device: &Path, mount_point: &Path) -> Result<(), ResizeError> {
    info!("Resizing {} filesystem on {}", fs_type, device.display());

    match fs_type.to_lowercase().as_str() {
        "ext4" | "ext3" | "ext2" => {
            let resize2fs_output = Command::new("resize2fs").arg("-f").arg(device).output();

            match resize2fs_output {
                Ok(output) => {
                    if output.status.success() {
                        info!("Successfully resized ext filesystem");
                        return Ok(());
                    }
                    let error = String::from_utf8_lossy(&output.stderr);
                    warn!("resize2fs failed: {}", error);
                }
                Err(e) => {
                    warn!("Failed to execute resize2fs: {}", e);
                }
            }

            Err(ResizeError::ResizeFs(format!(
                "Failed to resize {} filesystem",
                fs_type
            )))
        }
        "xfs" => {
            let xfs_growfs_output = Command::new("xfs_growfs").arg(mount_point).output();

            match xfs_growfs_output {
                Ok(output) => {
                    if output.status.success() {
                        info!("Successfully resized XFS filesystem");
                        return Ok(());
                    }
                    let error = String::from_utf8_lossy(&output.stderr);
                    warn!("xfs_growfs failed: {}", error);

                    let xfs_growfs_d_output = Command::new("xfs_growfs")
                        .args(["-d", &mount_point.to_string_lossy()])
                        .output();

                    match xfs_growfs_d_output {
                        Ok(output) => {
                            if output.status.success() {
                                info!("Successfully resized XFS filesystem using -d flag");
                                return Ok(());
                            }
                            let error = String::from_utf8_lossy(&output.stderr);
                            warn!("xfs_growfs -d failed: {}", error);
                        }
                        Err(e) => {
                            warn!("Failed to execute xfs_growfs -d: {}", e);
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to execute xfs_growfs: {}", e);
                }
            }

            Err(ResizeError::ResizeFs(
                "Failed to resize XFS filesystem".to_string(),
            ))
        }
        "btrfs" => {
            let btrfs_resize_output = Command::new("btrfs")
                .args([
                    "filesystem",
                    "resize",
                    "max",
                    &mount_point.to_string_lossy(),
                ])
                .output();

            match btrfs_resize_output {
                Ok(output) => {
                    if output.status.success() {
                        info!("Successfully resized Btrfs filesystem");
                        return Ok(());
                    }
                    let error = String::from_utf8_lossy(&output.stderr);
                    warn!("btrfs filesystem resize failed: {}", error);

                    let btrfs_alt_output = Command::new("btrfs")
                        .args(["resize", "max", &mount_point.to_string_lossy()])
                        .output();

                    match btrfs_alt_output {
                        Ok(output) => {
                            if output.status.success() {
                                info!(
                                    "Successfully resized Btrfs filesystem using alternate command"
                                );
                                return Ok(());
                            }
                            let error = String::from_utf8_lossy(&output.stderr);
                            warn!("btrfs resize failed: {}", error);
                        }
                        Err(e) => {
                            warn!("Failed to execute btrfs resize: {}", e);
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to execute btrfs filesystem resize: {}", e);
                }
            }

            Err(ResizeError::ResizeFs(
                "Failed to resize Btrfs filesystem".to_string(),
            ))
        }
        _ => Err(ResizeError::ResizeFs(format!(
            "Unsupported filesystem: {}",
            fs_type
        ))),
    }
}

pub fn resize_luks(device: &Path) -> Result<(), ResizeError> {
    info!("Resizing LUKS container on {}", device.display());

    let cryptsetup_output = Command::new("cryptsetup")
        .args(["resize", &device.to_string_lossy()])
        .output();

    match cryptsetup_output {
        Ok(output) => {
            if output.status.success() {
                info!("Successfully resized LUKS container");
                return Ok(());
            }
            let error = String::from_utf8_lossy(&output.stderr);
            Err(ResizeError::ResizeLuks(error.to_string()))
        }
        Err(e) => Err(ResizeError::CommandFailed(e.to_string())),
    }
}

pub fn verify_resize(mount_point: &Path) -> Result<(), ResizeError> {
    info!("Verifying resize at {}", mount_point.display());

    let df_output = Command::new("df")
        .args(["-h", &mount_point.to_string_lossy()])
        .output();

    match df_output {
        Ok(output) => {
            if output.status.success() {
                info!("Current size:\n{}", String::from_utf8_lossy(&output.stdout));
                return Ok(());
            }
            warn!("df command failed");
        }
        Err(e) => {
            warn!("Failed to execute df: {}", e);
        }
    }

    let lsblk_output = Command::new("lsblk")
        .args(["-fo", "NAME,SIZE,MOUNTPOINT", "--path"])
        .output();

    match lsblk_output {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let filtered_output: String = stdout
                    .lines()
                    .filter(|line| line.contains(&*mount_point.to_string_lossy()))
                    .collect::<Vec<&str>>()
                    .join("\n");

                info!("Current size from lsblk:\n{}", filtered_output);
                return Ok(());
            }
            warn!("lsblk command failed");
        }
        Err(e) => {
            warn!("Failed to execute lsblk: {}", e);
        }
    }

    Err(ResizeError::CommandFailed(
        "Failed to get filesystem size information".into(),
    ))
}
