{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    rustc
    cargo
    rustfmt
    clippy
    rust-analyzer
    pkg-config
    udev
    systemd
    util-linux
    e2fsprogs
    xfsprogs
    btrfs-progs
    cloud-utils
    cryptsetup
  ];

  shellHook = ''
    export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath [
      pkgs.systemd
    ]}:$LD_LIBRARY_PATH
  '';

  RUST_BACKTRACE = 1;
}
