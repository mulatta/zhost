{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.zotero;
in
{
  options.programs.zotero = {
    enable = lib.mkEnableOption "Zotero patched to sync against a self-hosted zhost server";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.zotero.override { inherit (cfg) apiUrl wwwUrl streamUrl; };
      defaultText = lib.literalExpression "pkgs.zotero.override { inherit (config.programs.zotero) apiUrl wwwUrl streamUrl; }";
      description = ''
        The Zotero package. Defaults to `pkgs.zotero` from the zhost overlay,
        patched with the configured endpoints; requires `zhost.overlays.default`
        to be applied to the nixpkgs instance.
      '';
    };

    apiUrl = lib.mkOption {
      type = lib.types.str;
      example = "https://zotero.example.org/";
      description = "Self-hosted server URL the client uses for the Zotero data API.";
    };

    wwwUrl = lib.mkOption {
      type = lib.types.str;
      example = "https://zotero.example.org/";
      description = "Self-hosted server URL for the www/account endpoints.";
    };

    streamUrl = lib.mkOption {
      type = lib.types.str;
      example = "wss://zotero.example.org/stream/";
      description = "Self-hosted server URL for the streaming (live sync) endpoint.";
    };

    applicationsDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.home.homeDirectory}/Applications";
      defaultText = lib.literalExpression ''"''${config.home.homeDirectory}/Applications"'';
      description = "darwin only: directory the signed Zotero.app is installed into at activation.";
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      # Linux: the patched bundle is a normal, runnable store path.
      (lib.mkIf pkgs.stdenv.hostPlatform.isLinux {
        home.packages = [ cfg.package ];
      })

      # darwin: the store bundle is unsigned (the build is kept pure), so it cannot
      # run from the store. Copy it into a writable location and deep-sign it on the
      # host at activation, where the system codesign is available.
      (lib.mkIf pkgs.stdenv.hostPlatform.isDarwin {
        home.activation.zoteroInstall = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
          run ${cfg.package.darwinInstall}/bin/zotero-darwin-install ${lib.escapeShellArg cfg.applicationsDir}
        '';
      })
    ]
  );
}
