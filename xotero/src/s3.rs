//! S3-compatible object storage for attachment bytes (Cloudflare R2 in
//! production, RustFS in the integration test).
//!
//! Local disk is not used. Uploads are streamed *through* this server: the
//! client POSTs the bytes here, the server verifies the md5, then PUTs the
//! object to the bucket. Downloads are served by redirecting the client to a
//! short-lived pre-signed GET URL, so the bytes are read straight from the
//! bucket and the read path never passes back through this server.

use s3::creds::Credentials;
use s3::{Bucket, Region};

// Re-exported so the crate's binary (whose own `s3` module shadows the extern
// crate name) can name the error type as `s3::S3Error`.
pub use s3::error::S3Error;

/// Connection settings for the bucket. `endpoint`/`region`/`path_style` cover
/// both R2 (`https://<account>.r2.cloudflarestorage.com`, region `auto`) and a
/// local RustFS/MinIO (`http://host:9000`), so the test and production paths use
/// one code path.
#[derive(Clone)]
pub struct Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub path_style: bool,
    /// Lifetime of a pre-signed download URL, in seconds.
    pub presign_ttl: u32,
}

pub struct Storage {
    bucket: Box<Bucket>,
    presign_ttl: u32,
}

impl Storage {
    pub fn new(cfg: &Config) -> Result<Self, S3Error> {
        let region = Region::Custom {
            region: cfg.region.clone(),
            endpoint: cfg.endpoint.clone(),
        };
        let creds = Credentials::new(
            Some(&cfg.access_key),
            Some(&cfg.secret_key),
            None,
            None,
            None,
        )?;
        let mut bucket = Bucket::new(&cfg.bucket, region, creds)?;
        // RustFS/MinIO need path-style addressing; R2 accepts it too.
        if cfg.path_style {
            bucket = bucket.with_path_style();
        }
        Ok(Storage {
            bucket,
            presign_ttl: cfg.presign_ttl,
        })
    }

    /// Store the object, failing on a non-success status from the backend.
    pub async fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), S3Error> {
        let response = self
            .bucket
            .put_object_with_content_type(key, bytes, content_type)
            .await?;
        let code = response.status_code();
        if !(200..300).contains(&code) {
            return Err(S3Error::HttpFailWithBody(
                code,
                String::from_utf8_lossy(response.bytes()).into_owned(),
            ));
        }
        Ok(())
    }

    /// A pre-signed GET URL the client can follow directly to the bucket.
    pub async fn presign_get(&self, key: &str) -> Result<String, S3Error> {
        self.bucket.presign_get(key, self.presign_ttl, None).await
    }
}
