use std::collections::HashMap;

use crate::domain::{LibraryId, Permissions, UserId};
use crate::s3;

pub(crate) struct Config {
    /// Static recovery token → access, loaded from secret files at boot.
    pub(crate) keys: HashMap<String, Permissions>,
    /// Dedicated stable secret for restart-safe browser login key derivation.
    pub(crate) login_kdf_key: Vec<u8>,
    pub(crate) user_id: UserId,
    pub(crate) username: String,
    pub(crate) display_name: String,
    /// Migration 0001 creates one legacy personal library with ID 1. Keeping its
    /// scope in application state makes every storage call explicit.
    pub(crate) library_id: LibraryId,
    pub(crate) bind: String,
    /// Client-facing base URL (e.g. the reverse-proxy address). Used for the
    /// login and upload URLs handed to the client, which must be reachable by it
    /// — not the internal bind address. (Downloads use a pre-signed bucket URL.)
    pub(crate) public_url: String,
    pub(crate) database_url: String,
    pub(crate) s3: s3::Config,
}

impl Config {
    pub(crate) fn from_env() -> Self {
        let keys = load_keys();
        let bind = std::env::var("ZHOST_BIND").unwrap_or_else(|_| "127.0.0.1:8189".into());
        let bind_address: std::net::SocketAddr =
            bind.parse().expect("ZHOST_BIND must be a socket address");
        assert!(
            bind_address.ip().is_loopback(),
            "ZHOST_BIND must be loopback so only the trusted local proxy can set OIDC headers"
        );
        let user_id = std::env::var("ZHOST_USER_ID")
            .ok()
            .and_then(|v| v.parse().ok())
            .and_then(UserId::new)
            .unwrap_or_else(|| UserId::new(1).expect("default user ID is positive"));
        let username = std::env::var("ZHOST_USERNAME").unwrap_or_else(|_| "zhost".into());
        let display_name = std::env::var("ZHOST_DISPLAY_NAME").unwrap_or_else(|_| "zhost".into());
        let library_id = LibraryId::new(1).expect("legacy library ID is positive");
        Self {
            keys,
            login_kdf_key: load_login_kdf_key(),
            user_id,
            username,
            display_name,
            library_id,
            public_url: std::env::var("ZHOST_PUBLIC_URL")
                .unwrap_or_else(|_| format!("http://{bind}")),
            bind,
            database_url: std::env::var("ZHOST_DATABASE_URL")
                .or_else(|_| std::env::var("DATABASE_URL"))
                .unwrap_or_else(|_| "postgres://localhost/zhost".into()),
            s3: load_s3(),
        }
    }
}

/// Build the token→access map from secret files. `ZHOST_KEYS` is a
/// comma-separated list of `<role>:<path>` entries (`rw`/`ro`), each path a
/// single-line token (a sops-nix secret exposed via systemd LoadCredential).
/// Falls back to a single read/write key from `ZHOST_API_KEY_FILE` /
/// `ZHOST_API_KEY` for simple deployments. Prefer files over the env, which is
/// visible in /proc.
fn load_keys() -> HashMap<String, Permissions> {
    let read_token = |path: &str| {
        std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read key file {path}: {e}"))
            .trim()
            .to_string()
    };
    let mut keys = HashMap::new();
    if let Ok(manifest) = std::env::var("ZHOST_KEYS") {
        for entry in manifest.split(',').filter(|s| !s.is_empty()) {
            let (role, path) = entry
                .split_once(':')
                .unwrap_or_else(|| panic!("ZHOST_KEYS entry not <role>:<path>: {entry}"));
            let write = role == "rw";
            let token = read_token(path);
            keys.insert(token, Permissions::recovery(write));
        }
    } else {
        let token = match std::env::var("ZHOST_API_KEY_FILE") {
            Ok(path) => read_token(&path),
            Err(_) => std::env::var("ZHOST_API_KEY").unwrap_or_else(|_| "zhost-dev-key".into()),
        };
        keys.insert(token, Permissions::recovery(true));
    }
    keys
}

fn load_login_kdf_key() -> Vec<u8> {
    let path = std::env::var("ZHOST_LOGIN_KDF_KEY_FILE")
        .expect("ZHOST_LOGIN_KDF_KEY_FILE must point to a stable login KDF credential");
    let mut key =
        std::fs::read(&path).unwrap_or_else(|error| panic!("read login KDF key {path}: {error}"));
    while matches!(key.last(), Some(b'\n' | b'\r')) {
        key.pop();
    }
    assert!(
        key.len() >= 32,
        "login KDF credential must contain at least 32 bytes"
    );
    key
}

/// Object storage settings from the environment. The access/secret keys prefer
/// a file (`*_FILE`, a systemd credential) over the raw env var, which is
/// visible in /proc — the same precedence as the API keys. `path_style` defaults
/// on (required by RustFS/MinIO, accepted by R2); `region` defaults to `auto`
/// (R2 ignores it). Defaults target a local RustFS for development.
fn load_s3() -> s3::Config {
    let from_file_or_env = |file: &str, var: &str| {
        std::env::var(file)
            .ok()
            .map(|path| {
                std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read S3 key file {path}: {e}"))
                    .trim()
                    .to_string()
            })
            .or_else(|| std::env::var(var).ok())
            .unwrap_or_default()
    };
    s3::Config {
        endpoint: std::env::var("ZHOST_S3_ENDPOINT")
            .unwrap_or_else(|_| "http://127.0.0.1:9000".into()),
        region: std::env::var("ZHOST_S3_REGION").unwrap_or_else(|_| "auto".into()),
        bucket: std::env::var("ZHOST_S3_BUCKET").unwrap_or_else(|_| "zotero".into()),
        access_key: from_file_or_env("ZHOST_S3_ACCESS_KEY_FILE", "ZHOST_S3_ACCESS_KEY"),
        secret_key: from_file_or_env("ZHOST_S3_SECRET_KEY_FILE", "ZHOST_S3_SECRET_KEY"),
        path_style: std::env::var("ZHOST_S3_PATH_STYLE")
            .map(|v| v != "false")
            .unwrap_or(true),
        // Short by default: the client follows the download redirect right away,
        // so the URL needn't stay valid long (it is an unauthenticated capability).
        presign_ttl: std::env::var("ZHOST_S3_PRESIGN_TTL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300),
    }
}
