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

    s3 = lib.mkOption {
      description = "S3-compatible object storage for attachment bytes (e.g. Cloudflare R2).";
      type = lib.types.submodule {
        options = {
          endpoint = lib.mkOption {
            type = lib.types.str;
            example = "https://<account>.r2.cloudflarestorage.com";
            description = "S3 endpoint URL.";
          };
          region = lib.mkOption {
            type = lib.types.str;
            default = "auto";
            description = "S3 region (R2 ignores it).";
          };
          bucket = lib.mkOption {
            type = lib.types.str;
            default = "zotero";
            description = "Bucket holding the attachment objects.";
          };
          accessKeyFile = lib.mkOption {
            type = lib.types.path;
            description = "File with the S3 access key ID (a secret), loaded as a credential.";
          };
          secretKeyFile = lib.mkOption {
            type = lib.types.path;
            description = "File with the S3 secret access key (a secret), loaded as a credential.";
          };
          pathStyle = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Path-style addressing (required by RustFS/MinIO; R2 accepts it).";
          };
          presignTtl = lib.mkOption {
            type = lib.types.ints.positive;
            default = 120;
            description = ''
              Lifetime (seconds) of a pre-signed download URL. Kept short: the
              client follows the redirect immediately, and the URL is an
              unauthenticated capability.
            '';
          };
        };
      };
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

    systemd.services.zhost = {
      description = "Self-hosted Zotero sync server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ] ++ lib.optional cfg.createLocalDatabase "postgresql.service";
      requires = lib.optional cfg.createLocalDatabase "postgresql.service";

      environment = {
        ZHOST_BIND = cfg.bind;
        ZHOST_PUBLIC_URL = cfg.publicUrl;
        ZHOST_DATABASE_URL = cfg.database;
        ZHOST_S3_ENDPOINT = cfg.s3.endpoint;
        ZHOST_S3_REGION = cfg.s3.region;
        ZHOST_S3_BUCKET = cfg.s3.bucket;
        ZHOST_S3_PATH_STYLE = lib.boolToString cfg.s3.pathStyle;
        ZHOST_S3_PRESIGN_TTL = toString cfg.s3.presignTtl;
        # %d expands to the systemd credentials directory at runtime.
        ZHOST_S3_ACCESS_KEY_FILE = "%d/s3-access-key";
        ZHOST_S3_SECRET_KEY_FILE = "%d/s3-secret-key";
        ZHOST_KEYS = keyManifest;
        RUST_LOG = "info";
      };

      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        User = cfg.user;
        Group = cfg.user;
        LoadCredential = (lib.mapAttrsToList (name: k: "${credName name}:${k.file}") allKeys) ++ [
          "s3-access-key:${cfg.s3.accessKeyFile}"
          "s3-secret-key:${cfg.s3.secretKeyFile}"
        ];
        Restart = "on-failure";

        # Hardening: the service keeps no local state — it needs only the PG
        # socket and outbound network (to the object store).
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
      };
    };
  };
}
