{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    rustc
    cargo
    cargo-audit
    rustfmt
    clippy
    rust-analyzer
    util-linux
    e2fsprogs
    xfsprogs
    btrfs-progs
    cryptsetup
  ];

  shellHook = ''
    rustfmt --edition 2024 src/*.rs
    cargo audit
  '';

  RUST_BACKTRACE = 1;
}
