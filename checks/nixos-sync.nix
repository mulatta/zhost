{ pkgs, self }:

# Headless VM running only the zhost server + postgres (no Zotero / no desktop);
# the test driver exercises the sync API directly over HTTP. The overlay is
# applied to the host pkgs (the VM inherits it read-only) rather than set on the
# node, which runNixOSTest forbids.
let
  testPkgs = pkgs.extend self.overlays.default;
in
testPkgs.testers.runNixOSTest {
  name = "zhost-sync";

  nodes.machine = {
    imports = [ self.nixosModules.zhost ];

    services.zhost = {
      enable = true;
      bind = "127.0.0.1:8189";
      publicUrl = "http://localhost:8189";
      apiKeyFile = testPkgs.writeText "zhost-test-key" "testtoken";
    };

    environment.systemPackages = [
      testPkgs.curl
      testPkgs.jq
    ];
  };

  testScript = builtins.readFile ./sync-test.py;
}
