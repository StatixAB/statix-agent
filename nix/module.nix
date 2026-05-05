{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.statix-agent;
  json = pkgs.formats.json {};
  defaultPackage = pkgs.callPackage ./package.nix {};
  generatedConfig = json.generate "statix-agent.json" cfg.settings;
  microvmRuntimeDeps = [
    pkgs.cloud-utils
    pkgs.openssh
    pkgs.qemu-utils
    pkgs.qemu_kvm
  ];
  microvmPreStartScript = pkgs.writeShellScript "statix-agent-microvm-pre-start" ''
    set -euo pipefail

    install -d -m 0700 -o ${cfg.user} -g ${cfg.group} ${cfg.stateDir} ${cfg.stateDir}/microvm ${cfg.stateDir}/microvm/projects
  '';
in
{
  options.services.statix-agent = {
    enable = lib.mkEnableOption "Statix agent";

    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage;
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix {}";
      description = "Statix agent package to run.";
    };

    configFile = lib.mkOption {
      type = lib.types.path;
      default = "/etc/statix/agent.json";
      description = "Path to the Statix agent JSON configuration file.";
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/statix-agent";
      description = "Persistent state directory used by the Statix agent.";
    };

    settings = lib.mkOption {
      type = lib.types.nullOr json.type;
      default = null;
      description = ''
        Agent configuration rendered as JSON. When unset, configFile is expected
        to be managed externally, for example by sops-nix or agenix.
      '';
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "statix-agent";
      description = "User that runs the Statix agent service.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "statix-agent";
      description = "Group that runs the Statix agent service.";
    };

    microvm = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Install and configure microVM runtime support for Statix project jobs.";
      };
    };

    wireguardTools = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Install WireGuard tooling used for node enrollment and optional tunnel setup.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.settings == null || cfg.configFile == "/etc/statix/agent.json";
        message = "services.statix-agent.settings currently writes /etc/statix/agent.json; use configFile with an externally managed file for other paths.";
      }
    ];

    users.groups = {
      ${cfg.group} = {};
    }
    // lib.optionalAttrs cfg.microvm.enable {
      kvm = {};
    };
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      home = cfg.stateDir;
      extraGroups = lib.optionals cfg.microvm.enable [ "kvm" ];
    };

    environment.systemPackages = [
      cfg.package
    ]
    ++ lib.optionals cfg.microvm.enable microvmRuntimeDeps
    ++ lib.optionals cfg.wireguardTools [
      pkgs.wireguard-tools
    ];

    environment.etc = lib.optionalAttrs (cfg.settings != null) {
      "statix/agent.json" = {
        source = generatedConfig;
        user = cfg.user;
        group = cfg.group;
        mode = "0400";
      };
    };

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir} 0700 ${cfg.user} ${cfg.group} - -"
    ]
    ++ lib.optionals cfg.microvm.enable [
      "d ${cfg.stateDir}/microvm 0700 ${cfg.user} ${cfg.group} - -"
      "d ${cfg.stateDir}/microvm/projects 0700 ${cfg.user} ${cfg.group} - -"
    ];

    systemd.services.statix-agent = {
      description = "Statix Agent";
      documentation = [ "https://statix.se/docs/nodes/nixos" ];
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];

      environment = {
        STATIX_AGENT_CONFIG = toString cfg.configFile;
        STATIX_AGENT_STATE_DIR = cfg.stateDir;
        HOME = cfg.stateDir;
      };

      path = lib.optionals cfg.microvm.enable microvmRuntimeDeps;

      serviceConfig = {
        Type = "simple";
        ExecStart = "${lib.getExe cfg.package} run";
        Restart = "always";
        RestartSec = "5s";
        KillSignal = "SIGINT";
        TimeoutStopSec = "20s";
        User = cfg.user;
        Group = cfg.group;
        WorkingDirectory = cfg.stateDir;
        RuntimeDirectory = "statix-agent";
        RuntimeDirectoryMode = "0700";
        ReadWritePaths = [ cfg.stateDir ];
        NoNewPrivileges = false;
        Delegate = lib.mkIf cfg.microvm.enable true;
        PrivateTmp = true;
        PrivateDevices = false;
        ProtectHome = "read-only";
        ProtectSystem = "strict";
        DeviceAllow = lib.mkIf cfg.microvm.enable [ "/dev/kvm rw" ];
        ProtectControlGroups = lib.mkIf cfg.microvm.enable false;
        ProtectKernelTunables = lib.mkIf cfg.microvm.enable false;
        ProtectKernelModules = true;
        RestrictNamespaces = lib.mkIf cfg.microvm.enable false;
      }
      // lib.optionalAttrs cfg.microvm.enable {
        ExecStartPre = "+${microvmPreStartScript}";
      };
    };
  };
}
