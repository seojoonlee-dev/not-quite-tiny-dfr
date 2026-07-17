# Drop-in replacement for nixpkgs' hardware.apple.touchBar (see
# <nixpkgs>/nixos/modules/hardware/apple-touchbar.nix), for hosts running
# not-quite-tiny-dfr instead of stock tiny-dfr. Same option interface
# (enable/package/settings), so existing config using that module doesn't
# need to change shape -- only what it actually does differs.
#
# Why this exists: the mainline module hardcodes the literal string
# "tiny-dfr" for both the config path (environment.etc."tiny-dfr/config.toml")
# and the systemd unit name (systemd.services.tiny-dfr.restartTriggers),
# instead of deriving either from `cfg.package`. Since this fork installs a
# unit named `not-quite-tiny-dfr.service` and reads
# `/etc/not-quite-tiny-dfr/config.toml`, pointing the mainline module's
# `package` at it produces no error -- `settings` just silently renders to a
# file the daemon never reads, and restartTriggers silently targets a unit
# that doesn't exist. This module derives both from the package's own
# `pname` instead, so it works for this fork (or any other differently-named
# one) without hardcoding a second literal.
#
# Also fixes a second, unrelated bug that bites any widget/Slider{Get,Set}
# command, regardless of `package`: those run as `sh -c "<command>"` (see
# src/widget.rs run_command), but NixOS' default per-service PATH additions
# (coreutils, findutils, gnugrep, gnused, systemd) do not include a shell --
# every `sh -c` invocation fails with ENOENT before it even looks at the
# command string, and the daemon discards the error silently (no log, no
# crash, the command just never runs). `extraPath` below defaults to
# including a shell for exactly this reason.
self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.hardware.apple.touchBar;
  format = pkgs.formats.toml {};
  cfgFile = format.generate "config.toml" cfg.settings;
  serviceName = cfg.package.pname;
in {
  disabledModules = ["hardware/apple-touchbar.nix"];

  options.hardware.apple.touchBar = {
    enable = lib.mkEnableOption "support for the Touch Bar on Apple laptops using not-quite-tiny-dfr";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "not-quite-tiny-dfr.packages.<system>.default";
      description = "The not-quite-tiny-dfr package to use.";
    };

    settings = lib.mkOption {
      type = format.type;
      default = {};
      description = ''
        Configuration for not-quite-tiny-dfr. See the README for available
        options: https://github.com/seojoonlee-dev/not-quite-tiny-dfr#configuration-options
      '';
      example = lib.literalExpression ''
        {
          MediaLayerDefault = true;
          EnablePixelShift = true;
        }
      '';
    };

    extraPath = lib.mkOption {
      type = lib.types.listOf (lib.types.either lib.types.package lib.types.str);
      default = [pkgs.bash];
      defaultText = lib.literalExpression "[ pkgs.bash ]";
      description = ''
        Extra entries for the daemon's systemd unit `path`, beyond NixOS'
        own default per-service additions (coreutils, findutils, gnugrep,
        gnused, systemd). Widget/Slider{Get,Set} commands run through a
        shell (see the module header), so this defaults to `pkgs.bash`;
        override (rather than extend) if a different shell should provide
        `sh`, or add further entries any command widgets need on `PATH`
        (e.g. a package that isn't referenced by absolute store path in
        `settings`).
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.packages = [cfg.package];
    services.udev.packages = [cfg.package];

    environment.etc."${serviceName}/config.toml".source = cfgFile;
    systemd.services.${serviceName} = {
      restartTriggers = [cfgFile];
      path = cfg.extraPath;
    };
  };
}
