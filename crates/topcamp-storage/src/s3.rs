//! [`BlobStore`] over any S3-compatible backend (RustFS in this repo).

use std::time::Duration;

use s3::{creds::Credentials, Bucket, BucketConfiguration, Region};

use crate::error::StorageError;
use crate::store::{BlobStore, ObjectMeta};

/// Connection facts. Region is cosmetic for path-style backends but the
/// signer requires one; credentials come from the deployment environment.
#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
}

impl S3Config {
    /// Local development / tests (compose.yml RustFS).
    pub fn dev() -> Self {
        Self {
            endpoint: std::env::var("RUSTFS_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:9000".to_string()),
            bucket: std::env::var("RUSTFS_BUCKET").unwrap_or_else(|_| "topcamp".to_string()),
            access_key: std::env::var("RUSTFS_ACCESS_KEY")
                .unwrap_or_else(|_| "rustfsadmin".to_string()),
            secret_key: std::env::var("RUSTFS_SECRET_KEY")
                .unwrap_or_else(|_| "change-me-in-production".to_string()),
            region: "us-east-1".to_string(),
        }
    }

    fn region(&self) -> Region {
        Region::Custom {
            region: self.region.clone(),
            endpoint: self.endpoint.clone(),
        }
    }

    fn credentials(&self) -> Result<Credentials, StorageError> {
        Credentials::new(
            Some(self.access_key.as_str()),
            Some(self.secret_key.as_str()),
            None,
            None,
            None,
        )
        .map_err(backend)
    }
}

/// S3-backed [`BlobStore`] with path-style addressing (RustFS/MinIO).
#[derive(Debug, Clone)]
pub struct S3BlobStore {
    bucket: Box<Bucket>,
}

impl S3BlobStore {
    pub fn new(config: &S3Config) -> Result<Self, StorageError> {
        let bucket =
            Bucket::new(&config.bucket, config.region(), config.credentials()?).map_err(backend)?;
        Ok(Self {
            bucket: bucket.with_path_style(),
        })
    }

    /// Create the bucket unless it exists (dev/test bootstrap).
    pub async fn ensure_bucket(&self, config: &S3Config) -> Result<(), StorageError> {
        match Bucket::create_with_path_style(
            &config.bucket,
            config.region(),
            config.credentials()?,
            BucketConfiguration::default(),
        )
        .await
        {
            Ok(_) => Ok(()),
            Err(err) => {
                // No typed "already exists"; the service message is stable.
                let message = err.to_string();
                if message.contains("BucketAlreadyOwnedByYou")
                    || message.contains("BucketAlreadyExists")
                {
                    Ok(())
                } else {
                    Err(backend(err))
                }
            }
        }
    }
}

fn backend<E: std::fmt::Display>(err: E) -> StorageError {
    StorageError::Backend(err.to_string())
}

fn is_missing(err: &s3::error::S3Error) -> bool {
    matches!(err, s3::error::S3Error::HttpFailWithBody(404, _))
}

#[allow(async_fn_in_trait)]
impl BlobStore for S3BlobStore {
    #[tracing::instrument(name = "blobstore.put", skip(self, body), fields(key = key, bytes = body.len()))]
    async fn put(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<(), StorageError> {
        let content_type = content_type.unwrap_or("application/octet-stream");
        self.bucket
            .put_object_with_content_type(key, &body, content_type)
            .await
            .map_err(backend)?;
        Ok(())
    }

    #[tracing::instrument(name = "blobstore.get", skip(self), fields(key = key))]
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        match self.bucket.get_object(key).await {
            Ok(response) => Ok(Some(response.to_vec())),
            Err(err) if is_missing(&err) => Ok(None),
            Err(err) => Err(backend(err)),
        }
    }

    #[tracing::instrument(name = "blobstore.head", skip(self), fields(key = key))]
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        match self.bucket.head_object(key).await {
            Ok((meta, status)) if (200..300).contains(&status) => Ok(Some(ObjectMeta {
                size: meta.content_length.map(|len| len.max(0) as u64),
                content_type: meta.content_type,
                etag: meta.e_tag,
            })),
            Ok((_, _)) => Ok(None),
            Err(err) if is_missing(&err) => Ok(None),
            Err(err) => Err(backend(err)),
        }
    }

    #[tracing::instrument(name = "blobstore.delete", skip(self), fields(key = key))]
    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.bucket.delete_object(key).await.map_err(backend)?;
        Ok(())
    }

    async fn presign_get(&self, key: &str, expires_in: Duration) -> Result<String, StorageError> {
        self.bucket
            .presign_get(key, expiry_secs(expires_in), None)
            .await
            .map_err(backend)
    }

    async fn presign_put(&self, key: &str, expires_in: Duration) -> Result<String, StorageError> {
        self.bucket
            .presign_put(key, expiry_secs(expires_in), None, None)
            .await
            .map_err(backend)
    }
}

/// SigV4 presigns cap at one week; clamp defensively.
fn expiry_secs(expires_in: Duration) -> u32 {
    expires_in.as_secs().min(604_800) as u32
}
