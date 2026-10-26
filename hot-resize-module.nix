{
  config,
  lib,
  pkgs,
  utils,
  ...
}:

let

  cfg = config.services.hotResize;
  devicesJson = builtins.toJSON (
    map (dev: {
      device = dev.device;
      fs_type = dev.fsType;
      mount_point = dev.mountPoint;
    }) cfg.devices
  );

  mountPoints = map (dev: dev.mountPoint) cfg.devices;
  hasMountPointIn = dir: lib.any (mp: mp == dir || lib.hasPrefix "${dir}/" mp) mountPoints;
  quoteSystemdPath = path: lib.replaceStrings [ "%" ] [ "%%" ] (builtins.toJSON path);

in

{
  options.services.hotResize = {
    enable = lib.mkEnableOption "the hot resize service for filesystems";

    package = lib.mkPackageOption pkgs "hot-resize" { };

    devices = lib.mkOption {
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            device = lib.mkOption {
              type = lib.types.strMatching "/dev/.+";
              example = "/dev/sda1";
              description = "Block device path to resize";
            };
            fsType = lib.mkOption {
              type = lib.types.enum [
                "ext4"
                "xfs"
                "btrfs"
              ];
              default = "ext4";
              description = "Filesystem type (supported: ext4, xfs, btrfs)";
            };
            mountPoint = lib.mkOption {
              type = lib.types.strMatching "/|(/[^/]+)+";
              example = "/";
              description = "Mount point of the filesystem to resize (absolute path without trailing slash)";
            };
          };
        }
      );
      default = [ ];
      example = lib.literalExpression ''
        [
          {
            device = "/dev/sda1";
            fsType = "ext4";
            mountPoint = "/";
          }
        ]
      '';
      description = "List of devices to monitor for resizing";
    };

    skipVerify = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Skip filesystem verification after resize";
    };

    daemon = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Enable daemon mode for automatic disk resizing";
    };

    checkInterval = lib.mkOption {
      type = lib.types.ints.positive;
      default = 60;
      description = "Check interval in seconds for daemon mode";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.tmpfiles.rules = [ "d /run/cryptsetup 0700 root root -" ];

    systemd.services.hotResize = {
      description = "Hot resize service for filesystems";
      wantedBy = [ "multi-user.target" ];
      requires = [ "local-fs-pre.target" ];
      after = [ "local-fs-pre.target" ];
      path = lib.optional config.services.lvm.enable config.services.lvm.package;
      serviceConfig = {
        Type = if cfg.daemon then "simple" else "oneshot";
        RemainAfterExit = false;
        ExecStart = utils.escapeSystemdExecArgs (
          [
            (lib.getExe cfg.package)
            "--devices"
            devicesJson
          ]
          ++ lib.optional cfg.skipVerify "--skip-verify"
          ++ lib.optionals cfg.daemon [
            "--auto"
            "--interval"
            cfg.checkInterval
          ]
        );

        CapabilityBoundingSet = [
          "CAP_SYS_ADMIN"
          "CAP_SYS_RESOURCE"
          "CAP_DAC_READ_SEARCH"
        ];
        
        NoNewPrivileges = true;
        ProtectSystem = if lib.elem "/" mountPoints then "full" else "strict";
        ProtectHome =
          !lib.any hasMountPointIn [
            "/home"
            "/root"
            "/run/user"
          ];
        ReadWritePaths =
          map (mp: quoteSystemdPath "-${mp}") mountPoints
          ++ [ "/run/cryptsetup" ]
          ++ lib.optionals config.services.lvm.enable [
            "-/etc/lvm"
            "-/run/lock/lvm"
          ];
        PrivateTmp = true;
        UMask = "0077";

        DevicePolicy = "closed";
        DeviceAllow = [
          "block-* rw"
          "/dev/mapper/control rw"
        ];

        ProtectClock = true;
        ProtectControlGroups = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectProc = "invisible";

        PrivateNetwork = true;
        RestrictAddressFamilies = [ "AF_UNIX" ];
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
          "~@resources"
        ];
        SystemCallErrorNumber = "EPERM";
      };
    };
  };
}
