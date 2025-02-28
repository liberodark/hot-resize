use clap::Parser;
use hot_resize::{analyze_device, check_requirements, resize};
use std::path::PathBuf;
use std::process::Command;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Safe disk resizing tool for Linux distributions",
    long_about = "Safely resize disk partitions and filesystems without rebooting"
)]
struct Args {
    /// Devices to resize in JSON format
    /// Example: '[{"device":"/dev/vda1","fs_type":"ext4","mount_point":"/"}]'
    #[arg(short, long)]
    devices: String,

    /// Skip filesystem verification after resize
    #[arg(short, long)]
    skip_verify: bool,

    /// Dry run mode - show what would be done without making changes
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Skip root user check (not recommended)
    #[arg(long)]
    no_root_check: bool,
}

#[derive(Debug, serde::Deserialize)]
struct Device {
    device: PathBuf,
    fs_type: FileSystem,
    mount_point: PathBuf,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum FileSystem {
    Ext4,
    Xfs,
    Btrfs,
}

impl FileSystem {
    fn as_str(&self) -> &'static str {
        match self {
            FileSystem::Ext4 => "ext4",
            FileSystem::Xfs => "xfs",
            FileSystem::Btrfs => "btrfs",
        }
    }
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        let output = Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    Some(uid == "0")
                } else {
                    None
                }
            });

        output.unwrap_or(false)
    }

    #[cfg(windows)]
    {
        false
    }
}

fn process_device(
    device: &Device,
    dry_run: bool,
    skip_verify: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Analyze device
    info!("Analyzing device: {:?}", device.device);
    let block_device = analyze_device(&device.device)?;

    info!("Device information:");
    info!("  Real device: {:?}", block_device.real_device);
    info!("  Disk: {}", block_device.disk_name);
    info!("  Partition: {}", block_device.partition_number);

    if dry_run {
        info!(
            "[DRY RUN] Would resize partition {} on disk /dev/{}",
            block_device.partition_number, block_device.disk_name
        );
        info!(
            "[DRY RUN] Would resize {} filesystem at {:?}",
            device.fs_type.as_str(),
            device.mount_point
        );
        return Ok(());
    }

    let is_luks = Command::new("cryptsetup")
        .args(["isLuks", &block_device.real_device.to_string_lossy()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    if is_luks {
        info!("Detected LUKS encrypted device");
    }

    // Grow partition
    resize::grow_partition(
        &format!("/dev/{}", block_device.disk_name),
        block_device.partition_number,
    )?;

    if is_luks {
        info!("Resizing LUKS container");
        let mapper_name = find_luks_mapper(&block_device.real_device)?;
        resize::resize_luks(&mapper_name)?;

        resize::resize_filesystem(&mapper_name, device.fs_type.as_str(), &device.mount_point)?;
    } else {
        resize::resize_filesystem(
            &block_device.real_device,
            device.fs_type.as_str(),
            &device.mount_point,
        )?;
    }

    // Verify resize if not skipped
    if !skip_verify {
        resize::verify_resize(&device.mount_point)?;
    }

    Ok(())
}

fn find_luks_mapper(device_path: &PathBuf) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let output = Command::new("lsblk")
        .args(["-lpno", "NAME", &device_path.to_string_lossy()])
        .output()?;

    let output_str = String::from_utf8_lossy(&output.stdout);
    for line in output_str.lines() {
        if line.contains("mapper") {
            return Ok(PathBuf::from(line.trim()));
        }
    }

    Err(format!("Could not find LUKS mapper device for {:?}", device_path).into())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();
    info!("Starting hot-resize v{}", env!("CARGO_PKG_VERSION"));

    // Parse command line arguments
    let args = Args::parse();

    // Check if running as root unless explicitly skipped
    if !args.no_root_check && !is_root() {
        error!("This program must be run as root. Use sudo or --no-root-check to skip this check (not recommended)");
        return Err("This program must be run as root".into());
    }

    // Parse devices from JSON
    info!("Parsing device configuration...");
    let devices: Vec<Device> = match serde_json::from_str(&args.devices) {
        Ok(devices) => devices,
        Err(e) => {
            error!("Failed to parse devices JSON: {}", e);
            return Err(e.into());
        }
    };

    if devices.is_empty() {
        warn!("No devices specified, nothing to do");
        return Ok(());
    }

    // Collect filesystem types to check required tools
    let fs_types: Vec<&str> = devices.iter().map(|dev| dev.fs_type.as_str()).collect();

    // Check required tools for these filesystem types
    info!("Checking for required tools...");
    match check_requirements(&fs_types) {
        Ok(_) => info!("All required tools are available"),
        Err(e) => {
            if args.dry_run {
                warn!(
                    "Missing tools detected (will continue in dry run mode): {}",
                    e
                );
            } else {
                error!("Tool check failed: {}", e);
                return Err(e.into());
            }
        }
    }

    // Process each device
    let mut success_count = 0;
    let total_devices = devices.len();

    for (i, device) in devices.iter().enumerate() {
        info!(
            "Processing device {}/{}: {:?}",
            i + 1,
            total_devices,
            device.device
        );
        match process_device(device, args.dry_run, args.skip_verify) {
            Ok(_) => {
                info!("Successfully processed device {:?}", device.device);
                success_count += 1;
            }
            Err(e) => {
                error!("Failed to process device {:?}: {}", device.device, e);
                // Continue with other devices
            }
        }
    }

    if success_count == total_devices {
        info!("Operation completed successfully for all devices");
    } else {
        info!(
            "Operation completed with {}/{} devices processed successfully",
            success_count, total_devices
        );
    }

    Ok(())
}
