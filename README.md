# hot-resize

A tool for hot resizing (without reboot) disk partitions and filesystems.

[![Rust](https://github.com/liberodark/hot-resize/actions/workflows/rust.yml/badge.svg)](https://github.com/liberodark/hot-resize/actions/workflows/rust.yml)

## Features

- Hot resizing of partitions without rebooting
- Support for ext4, XFS, and Btrfs filesystems
- LUKS encrypted container support
- Automatic verification of required tools
- Simple command-line interface with JSON input

## Prerequisites

### Required System Dependencies

The following tools must be installed on your system:
- `lsblk` (typically in util-linux package)
- `growpart` (typically in cloud-utils or cloud-guest-utils)
- `resize2fs` (for ext4, typically in e2fsprogs)
- `xfs_growfs` (for XFS, typically in xfsprogs)
- `btrfs` (for Btrfs, typically in btrfs-progs or btrfs-tools)
- `cryptsetup` (optional, for LUKS support)

### Installing Dependencies by Distribution

#### NixOS
```nix
environment.systemPackages = with pkgs; [
  util-linux
  cloud-utils
  e2fsprogs
  xfsprogs
  btrfs-progs
  cryptsetup
];
```

#### Debian/Ubuntu
```bash
sudo apt-get update
sudo apt-get install -y util-linux cloud-guest-utils e2fsprogs xfsprogs btrfs-progs cryptsetup-bin
```

#### Fedora/RHEL/CentOS
```bash
sudo dnf install -y util-linux cloud-utils e2fsprogs xfsprogs btrfs-progs cryptsetup
```

#### Arch Linux
```bash
sudo pacman -S util-linux cloud-utils e2fsprogs xfsprogs btrfs-progs cryptsetup
```

#### openSUSE
```bash
sudo zypper install util-linux cloud-utils e2fsprogs xfsprogs btrfs-progs cryptsetup
```

#### Alpine Linux
```bash
sudo apk add util-linux cloud-utils e2fsprogs xfsprogs btrfs-progs cryptsetup
```

## Installation

### Via cargo
```bash
cargo install --path .
```

### Manual build
```bash
git clone https://github.com/liberodark/hot-resize.git
cd hot-resize
cargo build --release
sudo cp target/release/hot-resize /usr/local/bin/
```

### Precompiled binaries
Precompiled binaries are available in the [Releases](https://github.com/liberodark/hot-resize/releases) section.

## Usage

The tool requires root privileges:

```bash
sudo hot-resize --devices '[{"device":"/dev/vda1","fs_type":"ext4","mount_point":"/"}]'
```

The `--devices` parameter accepts a JSON array containing:
- `device`: Path to the device to resize
- `fs_type`: Filesystem type (`ext4`, `xfs`, or `btrfs`)
- `mount_point`: Mount point of the filesystem

You can specify multiple devices in the same array:

```bash
sudo hot-resize --devices '[
  {"device":"/dev/vda1", "fs_type":"ext4", "mount_point":"/"},
  {"device":"/dev/vdb1", "fs_type":"xfs", "mount_point":"/data"}
]'
```

### Options
- `--dry-run`: Simulate operations without making changes
- `--skip-verify`: Skip verification after resizing

## NixOS Integration

A NixOS module is available to integrate hot-resize directly into your configuration:

```nix
# Add to your configuration.nix
imports = [
  ./path/to/hot-resize-module.nix
];

services.hot-resize = {
  enable = true;
  runAtBoot = true;
  devices = [
    {
      device = "/dev/vda1";
      fs_type = "ext4";
      mount_point = "/";
    }
  ];
};
```

See the [hot-resize-module.nix](./hot-resize-module.nix) file for more details.

## Troubleshooting

### Missing Dependencies
If you encounter errors regarding missing tools, use the `--dry-run` option to see which tools are needed for your configuration.

### Permission Issues
The tool must be run with root privileges. Make sure you're using `sudo` or logged in as root.

### LUKS Containers
For devices encrypted with LUKS, ensure that `cryptsetup` is installed and that devices are unlocked before running the tool.

## License

This project is distributed under the [GPL-3.0](LICENSE) license.
