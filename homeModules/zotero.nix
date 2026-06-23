# Curried with zhostpkgs (this flake's own pinned nixpkgs) so the base Zotero
# release is owned here, not inherited from the consumer's nixpkgs. A consumer
# whose nixpkgs races ahead of the published Zotero binaries (no dmg yet, no
# matching hash) therefore cannot break this module; the version only moves when
# this flake's lock is bumped together with the dmg hash in pkgs/zotero.
{ zhostpkgs }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.zotero;
  # Build tools (stdenv, zip, ...) still come from the consumer pkgs so they
  # match the host; only the Zotero base (version + linux build) is pinned.
  baseZotero = zhostpkgs.legacyPackages.${pkgs.stdenv.hostPlatform.system}.zotero;
in
{
  options.programs.zotero = {
    enable = lib.mkEnableOption "Zotero patched to sync against a self-hosted zhost server";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../pkgs/zotero {
        inherit (cfg)
          apiUrl
          wwwUrl
          streamUrl
          prefs
          ;
        # Pin the base to this flake's nixpkgs, not the consumer's.
        zotero = baseZotero;
      };
      defaultText = lib.literalExpression "pkgs.callPackage <zhost/pkgs/zotero> { inherit (config.programs.zotero) apiUrl wwwUrl streamUrl prefs; zotero = <zhost nixpkgs>.zotero; }";
      description = ''
        The Zotero package, built from this flake's patched derivation against
        the configured endpoints. The base Zotero release is pinned to this
        flake's own nixpkgs, so it stays buildable even when the consumer's
        nixpkgs jumps to a version whose binaries are not published yet.
        Self-contained: setting the endpoints is enough, with no need to apply
        `zhost.overlays.default` to nixpkgs. Override only for a custom build.
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

    prefs = lib.mkOption {
      type =
        with lib.types;
        attrsOf (oneOf [
          bool
          int
          str
        ]);
      default = { };
      example = lib.literalExpression ''{ "extensions.zotero.automaticTags" = false; }'';
      description = ''
        Zotero preferences baked into the bundle's default prefs. A later
        definition overrides the upstream default, so e.g. setting
        `"extensions.zotero.automaticTags" = false` stops metadata retrieval and
        import translators from attaching publisher subject keywords as automatic
        tags. The user can still change these in-app.
      '';
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
