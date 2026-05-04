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
  lxcHome = "${cfg.stateDir}/lxc";
  lxcConfigDir = "${lxcHome}/.config/lxc";
  lxcConfigFile = "${lxcConfigDir}/default.conf";
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

    lxc = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Install and configure LXC support for Statix container jobs.";
      };

      bridge = lib.mkOption {
        type = lib.types.str;
        default = "lxcbr0";
        description = "LXC bridge that statix-agent may attach veth devices to.";
      };

      usernetDevices = lib.mkOption {
        type = lib.types.ints.positive;
        default = 10;
        description = "Number of LXC veth devices allowed for the agent user.";
      };

      subIdStart = lib.mkOption {
        type = lib.types.ints.positive;
        default = 100000;
        description = "First subordinate uid/gid allocated to the agent user.";
      };

      subIdCount = lib.mkOption {
        type = lib.types.ints.positive;
        default = 65536;
        description = "Number of subordinate uid/gid values allocated to the agent user.";
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

    users.groups.${cfg.group} = {};
    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.group;
      home = cfg.stateDir;
    }
    // lib.optionalAttrs cfg.lxc.enable {
      subUidRanges = [
        {
          startUid = cfg.lxc.subIdStart;
          count = cfg.lxc.subIdCount;
        }
      ];
      subGidRanges = [
        {
          startGid = cfg.lxc.subIdStart;
          count = cfg.lxc.subIdCount;
        }
      ];
    };

    environment.systemPackages = [
      cfg.package
    ]
    ++ lib.optionals cfg.lxc.enable [
      pkgs.lxc
      pkgs.shadow
    ]
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
    }
    // lib.optionalAttrs cfg.lxc.enable {
      "lxc/lxc-usernet".text = "${cfg.user} veth ${cfg.lxc.bridge} ${toString cfg.lxc.usernetDevices}\n";
    };

    virtualisation.lxc.enable = lib.mkIf cfg.lxc.enable true;

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir} 0700 ${cfg.user} ${cfg.group} - -"
    ]
    ++ lib.optionals cfg.lxc.enable [
      "d ${lxcHome} 0711 ${cfg.user} ${cfg.group} - -"
      "d ${lxcHome}/containers 0711 ${cfg.user} ${cfg.group} - -"
      "d ${lxcHome}/.cache 0750 ${cfg.user} ${cfg.group} - -"
      "d ${lxcHome}/.local 0750 ${cfg.user} ${cfg.group} - -"
      "d ${lxcHome}/.local/share 0750 ${cfg.user} ${cfg.group} - -"
      "d ${lxcConfigDir} 0750 ${cfg.user} ${cfg.group} - -"
      "d /run/lxc 0755 root root - -"
      "d /run/lxc/nics 0755 root root - -"
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
      }
      // lib.optionalAttrs cfg.lxc.enable {
        HOME = cfg.stateDir;
      };

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
        ReadWritePaths = [ cfg.stateDir ] ++ lib.optional cfg.lxc.enable "-/run/lxc";
        NoNewPrivileges = false;
        Delegate = lib.mkIf cfg.lxc.enable true;
        PrivateTmp = true;
        PrivateDevices = false;
        ProtectHome = "read-only";
        ProtectSystem = "strict";
        ProtectControlGroups = lib.mkIf cfg.lxc.enable false;
        ProtectKernelTunables = lib.mkIf cfg.lxc.enable false;
        ProtectKernelModules = true;
        RestrictSUIDSGID = lib.mkIf cfg.lxc.enable false;
        RestrictNamespaces = lib.mkIf cfg.lxc.enable false;
      };

      preStart = lib.mkIf cfg.lxc.enable ''
        install -d -m 0711 -o ${cfg.user} -g ${cfg.group} ${cfg.stateDir} ${lxcHome} ${lxcHome}/containers
        install -d -m 0750 -o ${cfg.user} -g ${cfg.group} ${lxcHome}/.cache ${lxcHome}/.local ${lxcHome}/.local/share ${lxcConfigDir}
        {
          if [ -r /etc/lxc/default.conf ]; then
            printf 'lxc.include = /etc/lxc/default.conf\n'
          else
            printf 'lxc.net.0.type = veth\n'
            printf 'lxc.net.0.link = ${cfg.lxc.bridge}\n'
            printf 'lxc.net.0.flags = up\n'
          fi
          printf 'lxc.idmap = u 0 %s %s\n' ${toString cfg.lxc.subIdStart} ${toString cfg.lxc.subIdCount}
          printf 'lxc.idmap = g 0 %s %s\n' ${toString cfg.lxc.subIdStart} ${toString cfg.lxc.subIdCount}
          printf 'lxc.apparmor.profile = unconfined\n'
        } > ${lxcConfigFile}
        chown ${cfg.user}:${cfg.group} ${lxcConfigFile}
        chmod 0640 ${lxcConfigFile}
      '';
    };
  };
}
