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
    #[error("Failed to resize LUKS container: {0}")]
    ResizeLuks(String),
}

/// Detects the filesystem type of a device by reading superblock magic bytes.
///
/// Supported signatures:
/// - ext2/3/4: magic `0xEF53` at offset 1080 (distinguished via feature flags)
/// - XFS: magic `XFSB` at offset 0
/// - Btrfs: magic `_BHRfS_M` at offset 0x10040
/// - LUKS: magic `LUKS\xBA\xBE` at offset 0
pub fn get_fs_type(device: &Path) -> Result<String, ResizeError> {
    detect_fs_magic(device)
}

/// Reads filesystem magic bytes from a file or block device.
fn detect_fs_magic(path: &Path) -> Result<String, ResizeError> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)
        .map_err(|e| ResizeError::DeviceNotFound(format!("{}: {}", path.display(), e)))?;

    // XFS: offset 0, magic "XFSB" (4 bytes, big-endian)
    let mut buf4 = [0u8; 4];
    if file.read_exact(&mut buf4).is_ok() && &buf4 == b"XFSB" {
        return Ok("xfs".to_string());
    }

    // LUKS: offset 0, magic "LUKS\xBA\xBE" (6 bytes)
    let mut buf6 = [0u8; 6];
    if file.seek(SeekFrom::Start(0)).is_ok()
        && file.read_exact(&mut buf6).is_ok()
        && buf6 == *b"LUKS\xBA\xBE"
    {
        return Ok("crypto_LUKS".to_string());
    }

    // ext2/3/4: offset 1080, magic 0xEF53 (2 bytes, little-endian)
    let mut buf2 = [0u8; 2];
    if file.seek(SeekFrom::Start(1080)).is_ok()
        && file.read_exact(&mut buf2).is_ok()
        && u16::from_le_bytes(buf2) == 0xEF53
    {
        return detect_ext_version(&mut file);
    }

    // Btrfs: offset 0x10040 (65600), magic "_BHRfS_M" (8 bytes)
    let mut buf8 = [0u8; 8];
    if file.seek(SeekFrom::Start(0x10040)).is_ok()
        && file.read_exact(&mut buf8).is_ok()
        && &buf8 == b"_BHRfS_M"
    {
        return Ok("btrfs".to_string());
    }

    Err(ResizeError::ResizeFs(format!(
        "Failed to detect filesystem type for {}",
        path.display()
    )))
}

/// Distinguishes between ext2, ext3, and ext4 by reading feature flags.
///
/// - ext4: INCOMPAT_EXTENTS (0x0040) at offset 1124
/// - ext3: COMPAT_HAS_JOURNAL (0x0004) at offset 1116
/// - ext2: neither of the above
fn detect_ext_version(file: &mut std::fs::File) -> Result<String, ResizeError> {
    use std::io::{Read, Seek, SeekFrom};

    // Read incompat features at offset 1124
    let mut incompat = [0u8; 4];
    if file.seek(SeekFrom::Start(1124)).is_ok() && file.read_exact(&mut incompat).is_ok() {
        let incompat_features = u32::from_le_bytes(incompat);

        // EXT4_FEATURE_INCOMPAT_EXTENTS = 0x0040
        if incompat_features & 0x0040 != 0 {
            return Ok("ext4".to_string());
        }
    }

    // Read compat features at offset 1116
    let mut compat = [0u8; 4];
    if file.seek(SeekFrom::Start(1116)).is_ok() && file.read_exact(&mut compat).is_ok() {
        let compat_features = u32::from_le_bytes(compat);

        // EXT3_FEATURE_COMPAT_HAS_JOURNAL = 0x0004
        if compat_features & 0x0004 != 0 {
            return Ok("ext3".to_string());
        }
    }

    Ok("ext2".to_string())
}

/// Minimum growth threshold in bytes. If the partition can only grow by less
/// than this amount, it is considered already at maximum size.
const GROW_FUDGE_BYTES: u64 = 1024 * 1024; // 1 MiB, same as growpart

/// Sectors reserved for GPT secondary header and table.
const GPT_SECONDARY_SECTORS: u64 = 33;

/// Alignment boundary in bytes. Partition sizes are rounded down to multiples
/// of this value for optimal I/O alignment (matches growpart behavior).
const ALIGN_BYTES: u64 = 1024 * 1024; // 1 MiB

pub fn grow_partition(disk: &str, partition: Option<u32>) -> Result<(), ResizeError> {
    if partition.is_none() {
        info!("Device is a whole disk (not a partition), skipping partition resize");
        return Ok(());
    }

    let partition_num = partition.unwrap();
    info!("Growing partition {} on disk {}", partition_num, disk);

    // Step 1: Dump current partition table
    let dump = sfdisk_dump(disk)?;

    // Step 2: Parse disk geometry and partition info from dump
    let disk_info = parse_sfdisk_dump(&dump, disk, partition_num)?;

    // Step 3: Compute max_end considering other partitions and GPT secondary header
    let max_end = compute_max_end(&disk_info);

    // Step 4: Check if partition can actually grow
    if disk_info.pt_end >= max_end {
        info!(
            "Partition {} is already at maximum size (end={}, max={})",
            partition_num, disk_info.pt_end, max_end
        );
        return Ok(());
    }

    let growth_sectors = max_end - disk_info.pt_end;
    let growth_bytes = growth_sectors * disk_info.sector_size;
    if growth_bytes < GROW_FUDGE_BYTES {
        info!(
            "Partition {} could only grow by {} bytes (< {} fudge), skipping",
            partition_num, growth_bytes, GROW_FUDGE_BYTES
        );
        return Ok(());
    }

    let new_size = max_end - disk_info.pt_start + 1;
    info!(
        "Growing partition {}: start={} old_size={} new_size={} (gaining {} sectors)",
        partition_num, disk_info.pt_start, disk_info.pt_size, new_size, growth_sectors
    );

    // Step 5: Build modified dump with new size
    let new_dump = build_new_dump(&dump, &disk_info, new_size)?;

    // Step 6: Apply the new table via sfdisk with backup
    apply_sfdisk(disk, &new_dump)?;

    // Step 7: Notify kernel
    notify_kernel_partition_change(disk, partition_num);

    info!("Successfully grew partition {} on {}", partition_num, disk);
    Ok(())
}

/// Information about a partition parsed from sfdisk --dump output.
struct SfdiskDiskInfo {
    sector_num: u64,
    sector_size: u64,
    pt_start: u64,
    pt_size: u64,
    pt_end: u64,
    /// Start sectors of all other partitions (used to compute max_end).
    other_starts: Vec<u64>,
    /// Device path for this partition in the dump (e.g. "/dev/sda1").
    part_device: String,
    /// Whether the table format is GPT.
    is_gpt: bool,
}

/// Runs `sfdisk --dump <disk>` and returns its output.
fn sfdisk_dump(disk: &str) -> Result<String, ResizeError> {
    let output = Command::new("sfdisk")
        .args(["--dump", disk])
        .output()
        .map_err(|e| {
            ResizeError::GrowPartition(format!("Failed to execute sfdisk --dump: {}", e))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ResizeError::GrowPartition(format!(
            "sfdisk --dump failed: {}",
            stderr.trim_end()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Parses the output of `sfdisk --dump` and `sfdisk --list` to extract
/// disk geometry and partition info.
fn parse_sfdisk_dump(
    dump: &str,
    disk: &str,
    partition_num: u32,
) -> Result<SfdiskDiskInfo, ResizeError> {
    // Get disk geometry from sfdisk --list (first line gives total sectors)
    let list_output = Command::new("sfdisk")
        .args(["--list", "--unit=S", disk])
        .output()
        .map_err(|e| {
            ResizeError::GrowPartition(format!("Failed to execute sfdisk --list: {}", e))
        })?;

    let list_str = String::from_utf8_lossy(&list_output.stdout);

    // Parse first line: "Disk /dev/vda: 20 GiB, 21474836480 bytes, 41943040 sectors"
    let (sector_num, sector_size) = parse_disk_geometry(&list_str)?;

    // Determine partition device path (e.g. /dev/sda1 or /dev/nvme0n1p1)
    let part_device = find_partition_device(dump, disk, partition_num)?;

    // Parse partition start and size from dump lines
    // Format: "/dev/sda1 : start=     2048, size=    39999487, ..."
    let (pt_start, pt_size) = parse_partition_entry(dump, &part_device)?;
    let pt_end = pt_start + pt_size - 1;

    // Collect start sectors of all other partitions
    let other_starts = collect_other_starts(dump, &part_device);

    // Detect GPT
    let is_gpt = dump.contains("label: gpt");

    Ok(SfdiskDiskInfo {
        sector_num,
        sector_size,
        pt_start,
        pt_size,
        pt_end,
        other_starts,
        part_device,
        is_gpt,
    })
}

/// Parses disk geometry from `sfdisk --list` output.
/// Returns (total_sectors, sector_size).
fn parse_disk_geometry(list_output: &str) -> Result<(u64, u64), ResizeError> {
    // First line: "Disk /dev/vda: 20 GiB, 21474836480 bytes, 41943040 sectors"
    for line in list_output.lines() {
        if line.starts_with("Disk /") && line.contains("bytes") && line.contains("sectors") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // Find "NNNN bytes," and "NNNN sectors"
            let mut disk_bytes: Option<u64> = None;
            let mut sectors: Option<u64> = None;
            for (i, part) in parts.iter().enumerate() {
                if (*part == "bytes," || *part == "bytes") && i > 0 {
                    disk_bytes = parts[i - 1].parse().ok();
                }
                if *part == "sectors" && i > 0 {
                    sectors = parts[i - 1].parse().ok();
                }
            }
            if let (Some(bytes), Some(secs)) = (disk_bytes, sectors) {
                let sector_size = if secs > 0 { bytes / secs } else { 512 };
                return Ok((secs, sector_size));
            }
        }
    }
    Err(ResizeError::GrowPartition(
        "Failed to parse disk geometry from sfdisk --list output".to_string(),
    ))
}

/// Checks if a sfdisk dump line corresponds to a specific device.
///
/// Verifies that the line starts with the device path AND that the next
/// character is a separator (space, colon, or end of string), preventing
/// false matches like `/dev/sda1` matching a line for `/dev/sda12`.
fn line_matches_device(line: &str, device: &str) -> bool {
    if !line.starts_with(device) {
        return false;
    }
    line.as_bytes()
        .get(device.len())
        .is_none_or(|&c| c == b' ' || c == b':')
}

/// Finds the partition device path in the dump for the given partition number.
/// Handles both /dev/sda1 and /dev/nvme0n1p1 naming conventions.
fn find_partition_device(
    dump: &str,
    disk: &str,
    partition_num: u32,
) -> Result<String, ResizeError> {
    let part_str = partition_num.to_string();

    // Try direct: /dev/sda1
    let candidate1 = format!("{}{}", disk, part_str);
    // Try with p separator: /dev/nvme0n1p1
    let candidate2 = format!("{}p{}", disk, part_str);

    for line in dump.lines() {
        let trimmed = line.trim();
        if line_matches_device(trimmed, &candidate2) && trimmed.contains("start=") {
            return Ok(candidate2);
        }
        if line_matches_device(trimmed, &candidate1) && trimmed.contains("start=") {
            return Ok(candidate1);
        }
    }

    Err(ResizeError::GrowPartition(format!(
        "Partition {} not found in sfdisk dump for {}",
        partition_num, disk
    )))
}

/// Parses start= and size= values for a specific partition from sfdisk dump.
fn parse_partition_entry(dump: &str, part_device: &str) -> Result<(u64, u64), ResizeError> {
    for line in dump.lines() {
        if !line_matches_device(line.trim(), part_device) {
            continue;
        }

        let start = extract_sfdisk_field(line, "start=");
        let size = extract_sfdisk_field(line, "size=");

        if let (Some(s), Some(sz)) = (start, size) {
            return Ok((s, sz));
        }
    }

    Err(ResizeError::GrowPartition(format!(
        "Failed to parse start/size for {} in sfdisk dump",
        part_device
    )))
}

/// Extracts a numeric field value from an sfdisk dump line.
/// e.g. "start=     2048," → Some(2048)
fn extract_sfdisk_field(line: &str, field: &str) -> Option<u64> {
    let pos = line.find(field)?;
    let after = &line[pos + field.len()..];
    let value_str: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit() || c.is_ascii_whitespace())
        .collect();
    value_str.trim().parse().ok()
}

/// Collects start sectors of all partitions except the target one.
fn collect_other_starts(dump: &str, exclude_device: &str) -> Vec<u64> {
    let mut starts = Vec::new();
    for line in dump.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('/')
            && trimmed.contains("start=")
            && !line_matches_device(trimmed, exclude_device)
            && let Some(start) = extract_sfdisk_field(trimmed, "start=")
            && start > 0
        {
            starts.push(start);
        }
    }
    starts
}

/// Computes the maximum end sector for the partition, considering:
/// - Other partitions that start after this one
/// - GPT secondary header (33 sectors from end of disk)
/// - 1 MiB alignment (partition size rounded down to multiple of 1 MiB)
fn compute_max_end(info: &SfdiskDiskInfo) -> u64 {
    // Find the smallest start sector of any partition that starts after ours
    let next_part_start = info
        .other_starts
        .iter()
        .filter(|&&s| s > info.pt_end)
        .min()
        .copied();

    let mut max_end = match next_part_start {
        Some(next_start) => next_start - 1,
        None => info.sector_num - 1,
    };

    // Reserve space for GPT secondary header (same logic as growpart)
    if info.sector_num > GPT_SECONDARY_SECTORS && max_end > info.sector_num - GPT_SECONDARY_SECTORS
    {
        max_end = info.sector_num - GPT_SECONDARY_SECTORS - 1;
    }

    // Align partition size to 1 MiB boundary (same logic as growpart).
    // This rounds the partition size DOWN to the nearest 1 MiB multiple.
    let sectors_per_align = ALIGN_BYTES / info.sector_size;
    if sectors_per_align > 0 {
        let max_size = max_end + 1 - info.pt_start;
        let aligned_size = (max_size / sectors_per_align) * sectors_per_align;
        max_end = aligned_size + info.pt_start - 1;
    }

    max_end
}

/// Builds a new sfdisk dump with the updated partition size.
/// Also removes `last-lba:` for GPT to allow sfdisk to use the full disk.
fn build_new_dump(dump: &str, info: &SfdiskDiskInfo, new_size: u64) -> Result<String, ResizeError> {
    let mut result = String::with_capacity(dump.len());
    let mut replaced = false;

    for line in dump.lines() {
        if line_matches_device(line.trim(), &info.part_device) && !replaced {
            // Replace the size value, preserving original formatting.
            // sfdisk dump format: "size= <spaces><number>,"
            // We match "size=" followed by optional whitespace and the old size number.

            // Try to find "size=" in line and replace the number after it
            if let Some(size_pos) = line.find("size=") {
                let before = &line[..size_pos];
                let after_size_eq = &line[size_pos + 5..]; // skip "size="

                // Skip whitespace, then the old number
                let spaces: String = after_size_eq.chars().take_while(|c| *c == ' ').collect();
                let num_start = spaces.len();
                let num_str: String = after_size_eq[num_start..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();

                if num_str == info.pt_size.to_string() {
                    let remainder = &after_size_eq[num_start + num_str.len()..];
                    // Preserve the original field width
                    let old_field_len = spaces.len() + num_str.len();
                    let new_num_str = format!("{:>width$}", new_size, width = old_field_len);
                    result.push_str(before);
                    result.push_str("size=");
                    result.push_str(&new_num_str);
                    result.push_str(remainder);
                    replaced = true;
                } else {
                    result.push_str(line);
                }
            } else {
                result.push_str(line);
            }
        } else if info.is_gpt && line.starts_with("last-lba:") {
            // Skip last-lba line for GPT (same as growpart)
            continue;
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    if !replaced {
        return Err(ResizeError::GrowPartition(format!(
            "Failed to update size in sfdisk dump for {}",
            info.part_device
        )));
    }

    Ok(result)
}

/// Applies a modified sfdisk dump to the disk with backup.
fn apply_sfdisk(disk: &str, new_dump: &str) -> Result<(), ResizeError> {
    // Lock the disk to protect against udev races (same as growpart).
    // The lock is released when _disk_lock is dropped.
    let disk_lock = std::fs::File::open(disk)
        .ok()
        .and_then(|f| nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusive).ok());
    if disk_lock.is_none() {
        warn!("Failed to lock disk {}, continuing without lock", disk);
    }

    let mut child = Command::new("sfdisk")
        .args(["--no-reread", "--force", disk])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| ResizeError::GrowPartition(format!("Failed to execute sfdisk: {}", e)))?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(new_dump.as_bytes());
    }

    let output = child
        .wait_with_output()
        .map_err(|e| ResizeError::GrowPartition(format!("sfdisk wait failed: {}", e)))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        return Ok(());
    }

    // sfdisk may exit non-zero but still have written the table successfully,
    // with the BLKRRPART ioctl failing because the partition is mounted.
    if (stderr.contains("BLKRRPART") || stderr.contains("Device or resource busy"))
        && (stdout.contains("The partition table has been altered")
            || stdout.contains("new partition table"))
    {
        info!("sfdisk wrote partition table, kernel re-read deferred to partx");
        return Ok(());
    }

    warn!(
        "sfdisk failed (exit {:?}): {}",
        output.status.code(),
        stderr.trim_end()
    );
    Err(ResizeError::GrowPartition(
        "sfdisk failed to write partition table".to_string(),
    ))
}

/// Notifies the kernel of a partition size change using partx.
fn notify_kernel_partition_change(disk: &str, partition_num: u32) {
    let partx_output = Command::new("partx")
        .args(["--update", "--nr", &partition_num.to_string(), disk])
        .output();

    match partx_output {
        Ok(output) => {
            if output.status.success() {
                info!("Kernel partition table updated via partx");
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!("partx --update failed: {}", stderr.trim_end());
            }
        }
        Err(e) => {
            warn!("Failed to execute partx: {}", e);
        }
    }
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

    let stat = nix::sys::statvfs::statvfs(mount_point).map_err(|e| {
        ResizeError::CommandFailed(format!("statvfs failed on {:?}: {}", mount_point, e))
    })?;

    let block_size = stat.fragment_size() as u64;
    let total = stat.blocks() * block_size;
    let free = stat.blocks_free() * block_size;
    let available = stat.blocks_available() * block_size;
    let used = total.saturating_sub(free);

    info!("Filesystem at {:?}:", mount_point);
    info!("  Total:     {}", format_bytes(total));
    info!("  Used:      {}", format_bytes(used));
    info!("  Available: {}", format_bytes(available));

    Ok(())
}

/// Formats a byte count into a human-readable string (e.g. "1.5 GiB")
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.1} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};
    use std::path::Path;

    /// Creates a temporary file with specific bytes written at a given offset.
    fn create_fake_device(
        writes: &[(u64, &[u8])],
    ) -> (tempfile::NamedTempFile, std::path::PathBuf) {
        let mut file = tempfile::NamedTempFile::new().expect("Failed to create temp file");

        // Ensure the file is large enough for btrfs magic (offset 0x10040 + 8)
        file.seek(SeekFrom::Start(0x10048)).unwrap();
        file.write_all(&[0u8]).unwrap();

        for (offset, data) in writes {
            file.seek(SeekFrom::Start(*offset)).unwrap();
            file.write_all(data).unwrap();
        }

        file.flush().unwrap();
        let path = file.path().to_path_buf();
        (file, path)
    }

    #[test]
    fn test_format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn test_format_bytes_small() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn test_format_bytes_kib() {
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn test_format_bytes_mib() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(500 * 1024 * 1024), "500.0 MiB");
    }

    #[test]
    fn test_format_bytes_gib() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(format_bytes(20 * 1024 * 1024 * 1024), "20.0 GiB");
    }

    #[test]
    fn test_format_bytes_tib() {
        assert_eq!(format_bytes(1024 * 1024 * 1024 * 1024), "1.0 TiB");
    }

    #[test]
    fn test_verify_resize_root() {
        // Root filesystem should always be available
        let result = verify_resize(Path::new("/"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_resize_nonexistent_mount() {
        let result = verify_resize(Path::new("/nonexistent_mount_point_xyz"));
        assert!(result.is_err());
    }

    #[test]
    fn test_detect_xfs() {
        let (_file, path) = create_fake_device(&[(0, b"XFSB")]);
        let result = detect_fs_magic(&path).unwrap();
        assert_eq!(result, "xfs");
    }

    #[test]
    fn test_detect_luks() {
        let (_file, path) = create_fake_device(&[(0, b"LUKS\xBA\xBE")]);
        let result = detect_fs_magic(&path).unwrap();
        assert_eq!(result, "crypto_LUKS");
    }

    #[test]
    fn test_detect_ext4() {
        // ext4 magic at offset 1080 + extents feature flag at offset 1124
        let magic = 0xEF53u16.to_le_bytes();
        let incompat: [u8; 4] = 0x0040u32.to_le_bytes(); // INCOMPAT_EXTENTS
        let (_file, path) = create_fake_device(&[(1080, &magic), (1124, &incompat)]);
        let result = detect_fs_magic(&path).unwrap();
        assert_eq!(result, "ext4");
    }

    #[test]
    fn test_detect_ext3() {
        // ext3: has journal flag but no extents
        let magic = 0xEF53u16.to_le_bytes();
        let compat: [u8; 4] = 0x0004u32.to_le_bytes(); // COMPAT_HAS_JOURNAL
        let incompat: [u8; 4] = 0x0000u32.to_le_bytes(); // no extents
        let (_file, path) =
            create_fake_device(&[(1080, &magic), (1116, &compat), (1124, &incompat)]);
        let result = detect_fs_magic(&path).unwrap();
        assert_eq!(result, "ext3");
    }

    #[test]
    fn test_detect_ext2() {
        // ext2: no journal, no extents
        let magic = 0xEF53u16.to_le_bytes();
        let compat: [u8; 4] = 0x0000u32.to_le_bytes();
        let incompat: [u8; 4] = 0x0000u32.to_le_bytes();
        let (_file, path) =
            create_fake_device(&[(1080, &magic), (1116, &compat), (1124, &incompat)]);
        let result = detect_fs_magic(&path).unwrap();
        assert_eq!(result, "ext2");
    }

    #[test]
    fn test_detect_btrfs() {
        let (_file, path) = create_fake_device(&[(0x10040, b"_BHRfS_M")]);
        let result = detect_fs_magic(&path).unwrap();
        assert_eq!(result, "btrfs");
    }

    #[test]
    fn test_detect_unknown_fs() {
        // File with no recognizable magic bytes
        let (_file, path) = create_fake_device(&[]);
        let result = detect_fs_magic(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_detect_fs_nonexistent_file() {
        let result = detect_fs_magic(Path::new("/nonexistent_device_xyz"));
        assert!(matches!(result, Err(ResizeError::DeviceNotFound(_))));
    }

    #[test]
    fn test_detect_empty_file() {
        let file = tempfile::NamedTempFile::new().expect("Failed to create temp file");
        let result = detect_fs_magic(file.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_xfs_takes_priority_over_ext() {
        // If XFS magic is at offset 0 AND ext magic at 1080, XFS wins (checked first)
        let ext_magic = 0xEF53u16.to_le_bytes();
        let (_file, path) = create_fake_device(&[(0, b"XFSB"), (1080, &ext_magic)]);
        let result = detect_fs_magic(&path).unwrap();
        assert_eq!(result, "xfs");
    }

    #[test]
    fn test_extract_sfdisk_field_start() {
        let line = "/dev/vda1 : start=     2048, size=    39999487, type=83";
        assert_eq!(extract_sfdisk_field(line, "start="), Some(2048));
    }

    #[test]
    fn test_extract_sfdisk_field_size() {
        let line = "/dev/vda1 : start=     2048, size=    39999487, type=83";
        assert_eq!(extract_sfdisk_field(line, "size="), Some(39999487));
    }

    #[test]
    fn test_extract_sfdisk_field_missing() {
        let line = "/dev/vda1 : start=     2048, type=83";
        assert_eq!(extract_sfdisk_field(line, "size="), None);
    }

    #[test]
    fn test_extract_sfdisk_field_no_padding() {
        let line = "/dev/sda1 : start=2048, size=1048576, type=83";
        assert_eq!(extract_sfdisk_field(line, "start="), Some(2048));
        assert_eq!(extract_sfdisk_field(line, "size="), Some(1048576));
    }

    #[test]
    fn test_parse_disk_geometry_standard() {
        let output = "Disk /dev/vda: 20 GiB, 21474836480 bytes, 41943040 sectors\n\
                      Units: sectors of 1 * 512 = 512 bytes\n";
        let (sectors, sector_size) = parse_disk_geometry(output).unwrap();
        assert_eq!(sectors, 41943040);
        assert_eq!(sector_size, 512);
    }

    #[test]
    fn test_parse_disk_geometry_4k_sectors() {
        let output = "Disk /dev/sda: 1 TiB, 1099511627776 bytes, 268435456 sectors\n";
        let (sectors, sector_size) = parse_disk_geometry(output).unwrap();
        assert_eq!(sectors, 268435456);
        assert_eq!(sector_size, 4096);
    }

    #[test]
    fn test_parse_disk_geometry_no_disk_line() {
        let output = "Units: sectors of 1 * 512 = 512 bytes\n";
        let result = parse_disk_geometry(output);
        assert!(result.is_err());
    }

    const SAMPLE_GPT_DUMP: &str = "\
label: gpt
label-id: 12345678-1234-1234-1234-123456789ABC
device: /dev/vda
unit: sectors
first-lba: 34
last-lba: 41943006

/dev/vda1 : start=        2048, size=    39999487, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, uuid=AAAA
/dev/vda2 : start=    40001536, size=     1941504, type=0657FD6D-A4AB-43C4-84E5-0933C84B4F4F, uuid=BBBB
";

    const SAMPLE_MBR_DUMP: &str = "\
label: dos
label-id: 0x12345678
device: /dev/sda
unit: sectors

/dev/sda1 : start=        2048, size=    39999487, type=83, bootable
/dev/sda2 : start=    40001536, size=     1941504, type=82
";

    #[test]
    fn test_find_partition_device_standard() {
        let dev = find_partition_device(SAMPLE_MBR_DUMP, "/dev/sda", 1).unwrap();
        assert_eq!(dev, "/dev/sda1");
    }

    #[test]
    fn test_find_partition_device_second_partition() {
        let dev = find_partition_device(SAMPLE_MBR_DUMP, "/dev/sda", 2).unwrap();
        assert_eq!(dev, "/dev/sda2");
    }

    #[test]
    fn test_find_partition_device_nvme() {
        let nvme_dump = "/dev/nvme0n1p1 : start=        2048, size=    39999487, type=83\n\
                         /dev/nvme0n1p2 : start=    40001536, size=     1941504, type=82\n";
        let dev = find_partition_device(nvme_dump, "/dev/nvme0n1", 1).unwrap();
        assert_eq!(dev, "/dev/nvme0n1p1");
    }

    #[test]
    fn test_find_partition_device_not_found() {
        let result = find_partition_device(SAMPLE_MBR_DUMP, "/dev/sda", 5);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_partition_device_ambiguous_numbers() {
        // /dev/sda1 must NOT match /dev/sda12
        let dump = "\
/dev/sda1 : start=        2048, size=    10000000, type=83
/dev/sda12 : start=    10002048, size=     5000000, type=83
";
        let dev = find_partition_device(dump, "/dev/sda", 1).unwrap();
        assert_eq!(dev, "/dev/sda1");

        let dev = find_partition_device(dump, "/dev/sda", 12).unwrap();
        assert_eq!(dev, "/dev/sda12");
    }

    #[test]
    fn test_find_partition_device_ambiguous_nvme() {
        let dump = "\
/dev/nvme0n1p1 : start=        2048, size=    10000000, type=83
/dev/nvme0n1p12 : start=    10002048, size=     5000000, type=83
";
        let dev = find_partition_device(dump, "/dev/nvme0n1", 1).unwrap();
        assert_eq!(dev, "/dev/nvme0n1p1");

        let dev = find_partition_device(dump, "/dev/nvme0n1", 12).unwrap();
        assert_eq!(dev, "/dev/nvme0n1p12");
    }

    #[test]
    fn test_line_matches_device() {
        assert!(line_matches_device("/dev/sda1 : start=2048", "/dev/sda1"));
        assert!(line_matches_device("/dev/sda1: start=2048", "/dev/sda1"));
        assert!(!line_matches_device("/dev/sda12 : start=2048", "/dev/sda1"));
        assert!(!line_matches_device("/dev/sda10 : start=2048", "/dev/sda1"));
        assert!(line_matches_device("/dev/sda12 : start=2048", "/dev/sda12"));
    }

    #[test]
    fn test_parse_partition_entry_ambiguous() {
        let dump = "\
/dev/sda1 : start=        2048, size=    10000000, type=83
/dev/sda12 : start=    10002048, size=     5000000, type=83
";
        let (start, size) = parse_partition_entry(dump, "/dev/sda1").unwrap();
        assert_eq!(start, 2048);
        assert_eq!(size, 10000000);

        let (start, size) = parse_partition_entry(dump, "/dev/sda12").unwrap();
        assert_eq!(start, 10002048);
        assert_eq!(size, 5000000);
    }

    #[test]
    fn test_parse_partition_entry() {
        let (start, size) = parse_partition_entry(SAMPLE_MBR_DUMP, "/dev/sda1").unwrap();
        assert_eq!(start, 2048);
        assert_eq!(size, 39999487);
    }

    #[test]
    fn test_parse_partition_entry_second() {
        let (start, size) = parse_partition_entry(SAMPLE_MBR_DUMP, "/dev/sda2").unwrap();
        assert_eq!(start, 40001536);
        assert_eq!(size, 1941504);
    }

    #[test]
    fn test_parse_partition_entry_not_found() {
        let result = parse_partition_entry(SAMPLE_MBR_DUMP, "/dev/sda9");
        assert!(result.is_err());
    }

    #[test]
    fn test_collect_other_starts() {
        let starts = collect_other_starts(SAMPLE_MBR_DUMP, "/dev/sda1");
        assert_eq!(starts, vec![40001536]);
    }

    #[test]
    fn test_collect_other_starts_none() {
        // Only one partition, nothing else
        let dump = "/dev/sda1 : start=        2048, size=    39999487, type=83\n";
        let starts = collect_other_starts(dump, "/dev/sda1");
        assert!(starts.is_empty());
    }

    #[test]
    fn test_collect_other_starts_ambiguous() {
        // Excluding /dev/sda1 must NOT exclude /dev/sda12
        let dump = "\
/dev/sda1 : start=        2048, size=    10000000, type=83
/dev/sda12 : start=    10002048, size=     5000000, type=83
";
        let starts = collect_other_starts(dump, "/dev/sda1");
        assert_eq!(starts, vec![10002048]);

        let starts = collect_other_starts(dump, "/dev/sda12");
        assert_eq!(starts, vec![2048]);
    }

    #[test]
    fn test_compute_max_end_last_partition() {
        // Partition 1 is the only one before disk end
        let info = SfdiskDiskInfo {
            sector_num: 41943040,
            sector_size: 512,
            pt_start: 2048,
            pt_size: 39999487,
            pt_end: 2048 + 39999487 - 1,
            other_starts: vec![],
            part_device: "/dev/sda1".to_string(),
            is_gpt: false,
        };
        let max = compute_max_end(&info);
        // After GPT reservation (33 sectors) and 1 MiB alignment
        assert_eq!(max, 41940991);
    }

    #[test]
    fn test_compute_max_end_with_next_partition() {
        // Partition 2 starts at 40001536, so partition 1 can't go past 40001535
        let info = SfdiskDiskInfo {
            sector_num: 41943040,
            sector_size: 512,
            pt_start: 2048,
            pt_size: 100000,
            pt_end: 2048 + 100000 - 1,
            other_starts: vec![40001536],
            part_device: "/dev/sda1".to_string(),
            is_gpt: false,
        };
        let max = compute_max_end(&info);
        assert_eq!(max, 40001535); // next_start - 1
    }

    #[test]
    fn test_compute_max_end_gpt_reserves_33_sectors() {
        let info = SfdiskDiskInfo {
            sector_num: 100000,
            sector_size: 512,
            pt_start: 2048,
            pt_size: 50000,
            pt_end: 2048 + 50000 - 1,
            other_starts: vec![],
            part_device: "/dev/sda1".to_string(),
            is_gpt: true,
        };
        let max = compute_max_end(&info);
        // After GPT reservation and 1 MiB alignment (2048 sectors)
        assert_eq!(max, 98303);
    }

    #[test]
    fn test_build_new_dump_mbr() {
        let info = SfdiskDiskInfo {
            sector_num: 41943040,
            sector_size: 512,
            pt_start: 2048,
            pt_size: 39999487,
            pt_end: 2048 + 39999487 - 1,
            other_starts: vec![],
            part_device: "/dev/sda1".to_string(),
            is_gpt: false,
        };
        let new_dump = build_new_dump(SAMPLE_MBR_DUMP, &info, 41940000).unwrap();
        // Must contain the new size, preserving the original field width
        assert!(
            new_dump.contains("size=    41940000"),
            "new_dump was: {}",
            new_dump
        );
        // Old size must be gone
        assert!(!new_dump.contains("39999487"));
        // sda2 should be unchanged
        assert!(new_dump.contains("/dev/sda2"));
        assert!(new_dump.contains("size=     1941504"));
    }

    #[test]
    fn test_build_new_dump_gpt_removes_last_lba() {
        let info = SfdiskDiskInfo {
            sector_num: 41943040,
            sector_size: 512,
            pt_start: 2048,
            pt_size: 39999487,
            pt_end: 2048 + 39999487 - 1,
            other_starts: vec![40001536],
            part_device: "/dev/vda1".to_string(),
            is_gpt: true,
        };
        let new_dump = build_new_dump(SAMPLE_GPT_DUMP, &info, 39999999).unwrap();
        // last-lba line must be removed for GPT
        assert!(!new_dump.contains("last-lba:"));
        // label should still be present
        assert!(new_dump.contains("label: gpt"));
    }

    #[test]
    fn test_grow_partition_fudge_threshold() {
        // Verify the fudge constant matches growpart's default
        assert_eq!(GROW_FUDGE_BYTES, 1024 * 1024);
    }

    #[test]
    fn test_gpt_secondary_sectors() {
        // Verify GPT secondary header reservation matches growpart
        assert_eq!(GPT_SECONDARY_SECTORS, 33);
    }

    #[test]
    fn test_align_bytes() {
        // Verify alignment matches growpart (1 MiB)
        assert_eq!(ALIGN_BYTES, 1024 * 1024);
    }

    #[test]
    fn test_compute_max_end_alignment_512() {
        // 512-byte sectors: 1 MiB = 2048 sectors
        // Disk of 10001 sectors, start at 2048
        // Before alignment: max_end = 10001 - 33 - 1 = 9967
        // max_size = 9967 + 1 - 2048 = 7920
        // 7920 / 2048 = 3 (integer)
        // aligned_size = 3 * 2048 = 6144
        // max_end = 6144 + 2048 - 1 = 8191
        let info = SfdiskDiskInfo {
            sector_num: 10001,
            sector_size: 512,
            pt_start: 2048,
            pt_size: 4096,
            pt_end: 2048 + 4096 - 1,
            other_starts: vec![],
            part_device: "/dev/sda1".to_string(),
            is_gpt: false,
        };
        let max = compute_max_end(&info);
        assert_eq!(max, 8191);
        // Verify the resulting size is a multiple of 2048 (1 MiB in 512-byte sectors)
        assert_eq!((max + 1 - info.pt_start) % 2048, 0);
    }

    #[test]
    fn test_compute_max_end_alignment_4k() {
        // 4096-byte sectors: 1 MiB = 256 sectors
        // Disk of 5000 sectors, start at 256
        // Before alignment: max_end = 5000 - 33 - 1 = 4966
        // max_size = 4966 + 1 - 256 = 4711
        // 4711 / 256 = 18 (integer)
        // aligned_size = 18 * 256 = 4608
        // max_end = 4608 + 256 - 1 = 4863
        let info = SfdiskDiskInfo {
            sector_num: 5000,
            sector_size: 4096,
            pt_start: 256,
            pt_size: 2000,
            pt_end: 256 + 2000 - 1,
            other_starts: vec![],
            part_device: "/dev/sda1".to_string(),
            is_gpt: false,
        };
        let max = compute_max_end(&info);
        assert_eq!(max, 4863);
        // Verify the resulting size is a multiple of 256 (1 MiB in 4K sectors)
        assert_eq!((max + 1 - info.pt_start) % 256, 0);
    }

    #[test]
    fn test_compute_max_end_tiny_disk_no_underflow() {
        // Disk smaller than GPT_SECONDARY_SECTORS (33) must not panic
        let info = SfdiskDiskInfo {
            sector_num: 20,
            sector_size: 512,
            pt_start: 1,
            pt_size: 10,
            pt_end: 10,
            other_starts: vec![],
            part_device: "/dev/sda1".to_string(),
            is_gpt: false,
        };
        // Should not panic — the GPT reservation is skipped
        let _max = compute_max_end(&info);
    }
}
