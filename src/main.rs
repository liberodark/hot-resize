use clap::Parser;
use std::path::PathBuf;
use tracing::{info};
use hot_resize::{analyze_device, resize};

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Devices to resize in JSON format
    #[arg(short, long)]
    devices: String,
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

fn process_device(device: &Device) -> Result<(), Box<dyn std::error::Error>> {
    // Analyze device
    let block_device = analyze_device(&device.device)?;

    info!("Device information:");
    info!("  Real device: {:?}", block_device.real_device);
    info!("  Disk: {}", block_device.disk_name);
    info!("  Partition: {}", block_device.partition_number);

    // Resize filesystem
    resize::resize_filesystem(
        &block_device.real_device,
        device.fs_type.as_str(),
        &device.mount_point
    )?;

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let devices: Vec<Device> = serde_json::from_str(&args.devices)?;

    for device in devices {
        info!("Processing device: {:?}", device.device);
        process_device(&device)?;
    }

    Ok(())
}