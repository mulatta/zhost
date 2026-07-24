{ pkgs, self }:

let
  testPkgs = pkgs.extend self.overlays.default;
  legacyMigrations = testPkgs.linkFarm "zhost-v7-migrations" (
    map
      (path: {
        name = builtins.baseNameOf path;
        inherit path;
      })
      [
        ../server/migrations/0001_init.sql
        ../server/migrations/0002_files.sql
        ../server/migrations/0003_fulltext.sql
        ../server/migrations/0004_fulltext_search.sql
        ../server/migrations/0005_item_search.sql
        ../server/migrations/0006_item_columns.sql
        ../server/migrations/0007_item_columns_fixes.sql
      ]
  );
in
testPkgs.testers.runNixOSTest {
  name = "zhost-key-auth";

  nodes.machine = {
    imports = [ self.nixosModules.zhost ];

    services.zhost = {
      enable = true;
      bind = "127.0.0.1:8189";
      publicUrl = "http://localhost:8189";
      keys.recovery.file = testPkgs.writeText "zhost-recovery-key" "recoverytoken";
      bootstrapOidcIssuer = "https://id.example.test";
      bootstrapOidcSubject = "alice-subject";
      loginKdfKeyFile = testPkgs.writeText "zhost-login-kdf-key" "0123456789abcdef0123456789abcdef";
      s3 = {
        endpoint = "http://127.0.0.1:9000";
        region = "us-east-1";
        bucket = "zotero";
        accessKeyFile = testPkgs.writeText "zhost-s3-access" "rustfsadmin";
        secretKeyFile = testPkgs.writeText "zhost-s3-secret" "rustfsadmin";
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

    # Keep zhost stopped until the test has recreated a populated v7 database.
    systemd.services.zhost.wantedBy = testPkgs.lib.mkForce [ ];
    systemd.services.zhost.environment = {
      ZHOST_USER_ID = testPkgs.lib.mkForce "101";
      ZHOST_USERNAME = testPkgs.lib.mkForce "alice";
      ZHOST_DISPLAY_NAME = testPkgs.lib.mkForce "Alice";
    };

    environment.etc."zhost-test/migrations".source = legacyMigrations;
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
