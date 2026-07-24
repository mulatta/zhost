{ pkgs, self }:

let
  testPkgs = pkgs.extend self.overlays.default;
  legacyMigrations = testPkgs.linkFarm "xotero-v7-migrations" (
    map
      (path: {
        name = builtins.baseNameOf path;
        inherit path;
      })
      [
        ../xotero/migrations/0001_init.sql
        ../xotero/migrations/0002_files.sql
        ../xotero/migrations/0003_fulltext.sql
        ../xotero/migrations/0004_fulltext_search.sql
        ../xotero/migrations/0005_item_search.sql
        ../xotero/migrations/0006_item_columns.sql
        ../xotero/migrations/0007_item_columns_fixes.sql
      ]
  );
in
testPkgs.testers.runNixOSTest {
  name = "xotero-key-auth";

  nodes.machine = {
    imports = [ self.nixosModules.xotero ];

    services.xotero = {
      enable = true;
      bind = "127.0.0.1:8189";
      publicUrl = "http://localhost:8189";
      keys.recovery.file = testPkgs.writeText "xotero-recovery-key" "recoverytoken";
      s3 = {
        endpoint = "http://127.0.0.1:9000";
        region = "us-east-1";
        bucket = "zotero";
        accessKeyFile = testPkgs.writeText "xotero-s3-access" "rustfsadmin";
        secretKeyFile = testPkgs.writeText "xotero-s3-secret" "rustfsadmin";
      };
    };

    services.rustfs = {
      enable = true;
      environmentFile = toString (
        testPkgs.writeText "rustfs-secrets.env" ''
          RUSTFS_ACCESS_KEY=rustfsadmin
          RUSTFS_SECRET_KEY=rustfsadmin
        ''
      );
    };

    # Keep xotero stopped until the test has recreated a populated v7 database.
    systemd.services.xotero.wantedBy = testPkgs.lib.mkForce [ ];
    systemd.services.xotero.environment = {
      XOTERO_USER_ID = testPkgs.lib.mkForce "101";
      XOTERO_USERNAME = testPkgs.lib.mkForce "alice";
      XOTERO_DISPLAY_NAME = testPkgs.lib.mkForce "Alice";
    };

    environment.etc."xotero-test/migrations".source = legacyMigrations;
    environment.systemPackages = [
      testPkgs.curl
      testPkgs.jq
      testPkgs.minio-client
      testPkgs.postgresql
      testPkgs.sqlx-cli
    ];
  };

  testScript = builtins.readFile ./key-auth-test.py;
}
