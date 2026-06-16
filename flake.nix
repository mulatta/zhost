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
      };

      # programs.zotero — install + configure + (darwin) sign-on-activation.
      # Consumers must also apply overlays.default so pkgs.zotero is the patched
      # build the module expects.
      homeModules.zotero = ./homeModules/zotero.nix;
      homeModules.default = ./homeModules/zotero.nix;

      packages = eachSystem (
        { pkgs, ... }:
        {
          # Default build keeps upstream endpoints; consumers override the URLs.
          # Useful for `nix build`/eval smoke tests.
          zotero = pkgs.callPackage ./pkgs/zotero { };
          zhost = pkgs.callPackage ./pkgs/zhost { };
        }
      );

      checks = eachSystem (
        { system, ... }:
        {
          formatting = treefmtEval.${system}.config.build.check self;
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
