# NixOS module for Podspine. Import via the flake's nixosModules.default:
#
#   { inputs.podspine.url = "github:schubydoo/podspine"; }
#   imports = [ inputs.podspine.nixosModules.default ];
#   services.podspine = { enable = true; library = "/srv/audiobooks"; };
self:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.podspine;
in
{
  options.services.podspine = {
    enable = lib.mkEnableOption "Podspine audiobook-to-podcast server";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.podspine;
      defaultText = lib.literalExpression "podspine.packages.\${system}.podspine";
      description = "The podspine package to run.";
    };

    library = lib.mkOption {
      type = lib.types.path;
      description = "Path to the folder of audiobooks to scan (read-only).";
      example = "/srv/audiobooks";
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/podspine";
      description = "Directory for the SQLite index and split episode files.";
    };

    bind = lib.mkOption {
      type = lib.types.str;
      default = "0.0.0.0:8080";
      description = "Address to bind.";
    };

    baseUrl = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "http://nas.lan:8080";
      description = "External base URL used to build feed/audio links. Set this to the address podcast apps will actually reach.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the bind port in the firewall. Only do this on a trusted LAN — the browse UI enumerates your whole library.";
    };

    environment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "Extra environment variables (e.g. PODSPINE_DEFAULT_COVER_URL).";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.podspine = {
      description = "Podspine audiobook-to-podcast server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      environment = {
        PODSPINE_LIBRARY = toString cfg.library;
        PODSPINE_DATA_DIR = toString cfg.dataDir;
        PODSPINE_BIND = cfg.bind;
      }
      // lib.optionalAttrs (cfg.baseUrl != null) { PODSPINE_BASE_URL = cfg.baseUrl; }
      // cfg.environment;

      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        Restart = "on-failure";
        DynamicUser = true;
        StateDirectory = "podspine";
        ReadOnlyPaths = [ (toString cfg.library) ];
        # Hardening.
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
      };
    };

    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedTCPPorts = [ (lib.toInt (lib.last (lib.splitString ":" cfg.bind))) ];
    };
  };
}
