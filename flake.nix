{
  description = "zhost";

  inputs = {
    # keep-sorted start
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    # keep-sorted end
  };

  outputs =
    {
      self,
      nixpkgs,
      treefmt-nix,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      inherit (nixpkgs) lib;

      eachSystem =
        f:
        lib.genAttrs systems (
          system:
          f {
            inherit system;
            pkgs = nixpkgs.legacyPackages.${system};
          }
        );

      treefmtEval = eachSystem (
        { pkgs, ... }:
        treefmt-nix.lib.evalModule pkgs {
          projectRootFile = "flake.nix";
          programs = {
            deadnix.enable = true;
            keep-sorted.enable = true;
            nixfmt.enable = true;
            rustfmt = {
              enable = true;
              edition = "2021";
            };
            statix.enable = true;
          };
        }
      );
    in
    {
      # Replaces nixpkgs' zotero with the self-hosted-patched build. linux.nix
      # bases on prev.zotero, so passing it explicitly avoids infinite recursion.
      # Consumers inject their server with pkgs.zotero.override { apiUrl = ...; }.
      overlays.default = _final: prev: {
        zotero = prev.callPackage ./pkgs/zotero { inherit (prev) zotero; };
        zhost = prev.callPackage ./pkgs/zhost { };
        # S3 backend for the integration test only (production uses R2).
        rustfs = prev.callPackage ./pkgs/rustfs { };
      };

      # programs.zotero — install + configure + (darwin) sign-on-activation.
      # Consumers must also apply overlays.default so pkgs.zotero is the patched
      # build the module expects.
      homeModules.zotero = ./homeModules/zotero.nix;
      homeModules.default = ./homeModules/zotero.nix;

      # malt deployment: systemd service + local postgres + credential-loaded key.
      # The host wires the sops secret, wg bind and reverse proxy.
      nixosModules.zhost = ./nixosModules/zhost.nix;
      nixosModules.default = ./nixosModules/zhost.nix;

      packages = eachSystem (
        { pkgs, ... }:
        {
          # Default build keeps upstream endpoints; consumers override the URLs.
          # Useful for `nix build`/eval smoke tests.
          zotero = pkgs.callPackage ./pkgs/zotero { };
          zhost = pkgs.callPackage ./pkgs/zhost { };
          rustfs = pkgs.callPackage ./pkgs/rustfs { };
        }
      );

      checks = eachSystem (
        { system, pkgs, ... }:
        {
          formatting = treefmtEval.${system}.config.build.check self;
        }
        # nixosTest needs a linux VM, so wire it only on linux systems.
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          nixos-sync = import ./checks/nixos-sync.nix { inherit pkgs self; };
        }
      );

      formatter = eachSystem ({ system, ... }: treefmtEval.${system}.config.build.wrapper);

      devShells = eachSystem (
        { pkgs, ... }:
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.postgresql_16
              pkgs.sqlx-cli
            ];
          };
        }
      );
    };
}
