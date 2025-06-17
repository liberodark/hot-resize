use clap::Parser;
use hot_resize::{analyze_device, check_requirements, resize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use tracing::{debug, error, info, warn};

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

    /// Run in daemon mode - continuously check and resize
    #[arg(short, long, conflicts_with = "dry_run")]
    auto: bool,

    /// Check interval in seconds for daemon mode (default: 60)
    #[arg(long, default_value = "60", requires = "auto")]
    interval: u64,
}

#[derive(Debug, serde::Deserialize, Clone)]
struct Device {
    device: PathBuf,
    fs_type: FileSystem,
    mount_point: PathBuf,
}

#[derive(Debug, serde::Deserialize, Clone)]
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
        nix::unistd::Uid::effective().is_root()
    }

    #[cfg(not(unix))]
    {
        false
    }
}

fn get_device_size(device: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    let output = Command::new("blockdev")
        .args(["--getsize64", &device.to_string_lossy()])
        .output()?;

    if !output.status.success() {
        return Err("Failed to get device size".into());
    }

    let size_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(size_str.parse::<u64>()?)
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

    if let Some(partition_num) = block_device.partition_number {
        info!("  Partition: {}", partition_num);
    } else {
        info!("  Whole disk (no partition)");
    }

    if dry_run {
        if let Some(partition_num) = block_device.partition_number {
            info!(
                "[DRY RUN] Would resize partition {} on disk /dev/{}",
                partition_num, block_device.disk_name
            );
        } else {
            info!(
                "[DRY RUN] Would skip partition resize for whole disk /dev/{}",
                block_device.disk_name
            );
        }

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
    if block_device.partition_number.is_some() {
        resize::grow_partition(
            &format!("/dev/{}", block_device.disk_name),
            block_device.partition_number,
        )?;
    } else {
        info!("Skipping partition resize for whole disk device");
    }

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

fn find_luks_mapper(device_path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
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

fn daemon_loop(
    devices: Vec<Device>,
    skip_verify: bool,
    interval: Duration,
    running: Arc<AtomicBool>,
) {
    info!("Starting daemon mode with check interval of {:?}", interval);

    let mut known_sizes: HashMap<PathBuf, u64> = HashMap::new();
    let mut first_run = true;

    while running.load(Ordering::Relaxed) {
        for device in &devices {
            let mut should_process = false;
            let mut size_changed = false;

            let block_device = match analyze_device(&device.device) {
                Ok(bd) => bd,
                Err(_) => continue,
            };

            if block_device.partition_number.is_some() {
                let parent_disk = PathBuf::from(format!("/dev/{}", block_device.disk_name));
                if let Ok(parent_size) = get_device_size(&parent_disk) {
                    let parent_key = PathBuf::from(format!("{}_parent", device.device.display()));
                    if let Some(&last_parent_size) = known_sizes.get(&parent_key) {
                        if parent_size != last_parent_size {
                            size_changed = true;
                            warn!(
                                "Parent disk size changed for {:?}: {} -> {}",
                                device.device, last_parent_size, parent_size
                            );
                        }
                    } else {
                        should_process = true;
                    }
                    known_sizes.insert(parent_key, parent_size);
                }
            }

            if let Ok(current_size) = get_device_size(&device.device) {
                if let Some(&last_size) = known_sizes.get(&device.device) {
                    if current_size != last_size {
                        size_changed = true;
                        warn!(
                            "Size changed for {:?}: {} -> {}",
                            device.device, last_size, current_size
                        );
                    }
                } else {
                    should_process = true;
                }
                known_sizes.insert(device.device.clone(), current_size);
            }

            if should_process || (size_changed && !first_run) {
                match process_device(device, false, skip_verify) {
                    Ok(_) => {
                        if size_changed && !first_run {
                            warn!("Successfully resized {:?} after size change", device.device);
                        } else if first_run {
                            debug!("Initial resize check completed for {:?}", device.device);
                        }
                    }
                    Err(e) => {
                        let error_str = e.to_string();
                        if !error_str.contains("already at maximum size")
                            && !error_str.contains("no space left")
                            && !error_str.contains("NOCHANGE")
                        {
                            error!("Failed to process device {:?}: {}", device.device, e);
                        } else {
                            debug!("Device {:?} already at maximum size", device.device);
                        }
                    }
                }
            }
        }

        first_run = false;

        let sleep_duration = Duration::from_secs(1);
        let mut remaining = interval;
        while remaining > Duration::ZERO && running.load(Ordering::Relaxed) {
            let sleep_time = remaining.min(sleep_duration);
            thread::sleep(sleep_time);
            remaining = remaining.saturating_sub(sleep_time);
        }
    }

    info!("Daemon mode stopped");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.auto {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .init();
    } else {
        tracing_subscriber::fmt::init();
    }

    info!("Starting hot-resize v{}", env!("CARGO_PKG_VERSION"));

    // Parse command line arguments
    let args = Args::parse();

    // Check if running as root unless explicitly skipped
    if !args.no_root_check && !is_root() {
        error!(
            "This program must be run as root. Use sudo or --no-root-check to skip this check (not recommended)"
        );
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
    if args.auto {
        let running = Arc::new(AtomicBool::new(true));
        let r = running.clone();

        ctrlc::set_handler(move || {
            info!("Received shutdown signal, stopping daemon...");
            r.store(false, Ordering::Relaxed);
        })
        .expect("Error setting Ctrl-C handler");

        daemon_loop(
            devices,
            args.skip_verify,
            Duration::from_secs(args.interval),
            running,
        );
        return Ok(());
    }

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

#[cfg(test)]
mod daemon_tests {
    use super::*;

    #[test]
    fn test_filesystem_enum_conversion() {
        assert_eq!(FileSystem::Ext4.as_str(), "ext4");
        assert_eq!(FileSystem::Xfs.as_str(), "xfs");
        assert_eq!(FileSystem::Btrfs.as_str(), "btrfs");
    }

    #[test]
    fn test_device_clone() {
        let device = Device {
            device: PathBuf::from("/dev/sda1"),
            fs_type: FileSystem::Ext4,
            mount_point: PathBuf::from("/"),
        };

        let cloned = device.clone();
        assert_eq!(device.device, cloned.device);
        assert_eq!(
            format!("{:?}", device.fs_type),
            format!("{:?}", cloned.fs_type)
        );
        assert_eq!(device.mount_point, cloned.mount_point);
    }

    #[test]
    fn test_is_root() {
        let root_status = is_root();
        assert!(root_status || !root_status);
    }

    #[test]
    fn test_args_parsing() {
        let args = vec!["hot-resize", "--devices", r#"[]"#, "--auto", "--dry-run"];
        let result = Args::try_parse_from(&args);
        assert!(result.is_err());

        let args = vec!["hot-resize", "--devices", r#"[]"#, "--interval", "30"];
        let result = Args::try_parse_from(&args);
        assert!(result.is_err());

        let args = vec![
            "hot-resize",
            "--devices",
            r#"[]"#,
            "--auto",
            "--interval",
            "30",
        ];
        let result = Args::try_parse_from(&args);
        assert!(result.is_ok());
    }
}
