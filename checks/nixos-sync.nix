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
      keys = {
        app.file = testPkgs.writeText "zhost-app-key" "testtoken";
        cli = {
          file = testPkgs.writeText "zhost-cli-key" "readonlytoken";
          readOnly = true;
        };
      };
      # Only this SSO identity (as forwarded by the front proxy) may authorize a
      # login; the test simulates the proxy by sending the header directly.
      loginAuthorizedUser = "owner@mulatta.io";
      # Attachment bytes go to the local RustFS standing in for S3/R2.
      s3 = {
        endpoint = "http://127.0.0.1:9000";
        region = "us-east-1";
        bucket = "zotero";
        accessKeyFile = testPkgs.writeText "zhost-s3-access" "rustfsadmin";
        secretKeyFile = testPkgs.writeText "zhost-s3-secret" "rustfsadmin";
      };
    };

    # RustFS: an S3-compatible store for the attachment upload/download paths.
    systemd.services.rustfs = {
      wantedBy = [ "multi-user.target" ];
      before = [ "zhost.service" ];
      serviceConfig = {
        ExecStart = "${testPkgs.rustfs}/bin/rustfs --address 127.0.0.1:9000 --access-key rustfsadmin --secret-key rustfsadmin /var/lib/rustfs";
        StateDirectory = "rustfs";
        Restart = "on-failure";
      };
    };
    systemd.services.zhost.after = [ "rustfs.service" ];

    environment.systemPackages = [
      testPkgs.curl
      testPkgs.jq
      testPkgs.gzip
      # `mc` creates the bucket before the upload subtest.
      testPkgs.minio-client
    ];
  };

  testScript = builtins.readFile ./sync-test.py;
}
