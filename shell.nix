{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    rustc
    cargo
    cargo-audit
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
    rustfmt --edition 2024 src/*.rs
    cargo audit
  '';

  RUST_BACKTRACE = 1;
}
