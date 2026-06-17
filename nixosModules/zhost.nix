{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.zhost;

  # Fold the apiKeyFile shorthand into the keys set.
  allKeys =
    cfg.keys
    // lib.optionalAttrs (cfg.apiKeyFile != null) {
      default = {
        file = cfg.apiKeyFile;
        readOnly = false;
      };
    };
  credName = name: "key-${name}";
  # ZHOST_KEYS: "<role>:%d/<cred>" entries; %d is the systemd credentials dir.
  keyManifest = lib.concatStringsSep "," (
    lib.mapAttrsToList (name: k: "${if k.readOnly then "ro" else "rw"}:%d/${credName name}") allKeys
  );
in
{
  options.services.zhost = {
    enable = lib.mkEnableOption "self-hosted Zotero sync server";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.zhost;
      defaultText = lib.literalExpression "pkgs.zhost";
      description = "The zhost package (from zhost's overlay).";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "zhost";
      description = "User the service runs as, and the PostgreSQL role (peer auth).";
    };

    bind = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:8189";
      description = "Internal listen address; front it with a reverse proxy.";
    };

    publicUrl = lib.mkOption {
      type = lib.types.str;
      example = "https://zotero.example.org";
      description = ''
        Client-facing base URL (the reverse-proxy address). Used for the login,
        upload and download URLs handed to the client. No trailing slash.
      '';
    };

    database = lib.mkOption {
      type = lib.types.str;
      default = "postgres:///zhost?host=/run/postgresql";
      description = "PostgreSQL connection URL (defaults to the local peer socket).";
    };

    storageDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/zhost/storage";
      description = "Directory for attachment file bytes. Encrypt at the host/FS layer.";
    };

    keys = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule {
          options = {
            file = lib.mkOption {
              type = lib.types.path;
              description = "File holding this key's single-line token (e.g. a sops-nix secret).";
            };
            readOnly = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Reject writes for this key (reads still work).";
            };
          };
        }
      );
      default = { };
      example = lib.literalExpression ''
        {
          app.file = config.sops.secrets.zhost-app-key.path;
          cli = {
            file = config.sops.secrets.zhost-cli-key.path;
            readOnly = true;
          };
        }
      '';
      description = ''
        Named API keys, each loaded from a secret file as a systemd credential
        and held in memory (never in the store or database). The token bytes stay
        in the secret; the access level is declared here. The login session is
        handed the first read/write key. At least one read/write key is required.
      '';
    };

    apiKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Shorthand for a single read/write key (equivalent to one `keys` entry).
        Use `keys` instead when you also need a read-only key. Loaded as a systemd
        credential, never placed in the store.
      '';
    };

    createLocalDatabase = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Provision the database/role on the local PostgreSQL.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = allKeys != { };
        message = "services.zhost: configure at least one key via `keys` or `apiKeyFile`.";
      }
      {
        assertion = lib.any (k: !k.readOnly) (lib.attrValues allKeys);
        message = "services.zhost: at least one key must be read/write (the app login needs it).";
      }
    ];

    services.postgresql = lib.mkIf cfg.createLocalDatabase {
      enable = true;
      ensureDatabases = [ "zhost" ];
      ensureUsers = [
        {
          name = cfg.user;
          ensureDBOwnership = true;
        }
      ];
    };

    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.user;
    };
    users.groups.${cfg.user} = { };

    # ReadWritePaths below requires the storage directory to already exist.
    systemd.tmpfiles.rules = [
      "d ${cfg.storageDir} 0750 ${cfg.user} ${cfg.user} -"
    ];

    systemd.services.zhost = {
      description = "Self-hosted Zotero sync server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ] ++ lib.optional cfg.createLocalDatabase "postgresql.service";
      requires = lib.optional cfg.createLocalDatabase "postgresql.service";

      environment = {
        ZHOST_BIND = cfg.bind;
        ZHOST_PUBLIC_URL = cfg.publicUrl;
        ZHOST_DATABASE_URL = cfg.database;
        ZHOST_STORAGE_DIR = cfg.storageDir;
        # %d expands to the systemd credentials directory at runtime.
        ZHOST_KEYS = keyManifest;
        RUST_LOG = "info";
      };

      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        User = cfg.user;
        Group = cfg.user;
        LoadCredential = lib.mapAttrsToList (name: k: "${credName name}:${k.file}") allKeys;
        Restart = "on-failure";

        # Hardening: the service only needs its state dir and the PG socket.
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        NoNewPrivileges = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        ReadWritePaths = [ cfg.storageDir ];
      };
    };
  };
}
