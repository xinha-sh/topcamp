//! BlobStore contract tests against live RustFS (compose.yml).
//!
//! Requires the `topcamp` bucket reachable at `RUSTFS_ENDPOINT`
//! (defaults mirror [`S3Config::dev`]); `ensure_bucket` creates it.

use topcamp_storage::{BlobStore, S3BlobStore, S3Config, PRESIGN_EXPIRY};

async fn store() -> S3BlobStore {
    let config = S3Config::dev();
    let store = S3BlobStore::new(&config).expect("store builds");
    store.ensure_bucket(&config).await.expect("bucket ready");
    store
}

#[tokio::test]
async fn put_get_roundtrip() {
    let store = store().await;
    let key = "contract/roundtrip.bin";
    let body = b"hello rustfs".to_vec();
    store
        .put(key, body.clone(), Some("application/octet-stream"))
        .await
        .unwrap();
    assert_eq!(store.get(key).await.unwrap(), Some(body));
    store.delete(key).await.unwrap();
    assert_eq!(store.get(key).await.unwrap(), None);
}

#[tokio::test]
async fn head_reports_facts_and_missing() {
    let store = store().await;
    let key = "contract/head.txt";
    store
        .put(key, b"12345".to_vec(), Some("text/plain"))
        .await
        .unwrap();
    let meta = store.head(key).await.unwrap().expect("meta found");
    assert_eq!(meta.size, Some(5));
    assert_eq!(meta.content_type.as_deref(), Some("text/plain"));
    assert!(meta.etag.is_some(), "etag present");
    assert_eq!(store.head("contract/nope-missing").await.unwrap(), None);
    store.delete(key).await.unwrap();
}

#[tokio::test]
async fn delete_is_idempotent() {
    let store = store().await;
    store.delete("contract/never-existed").await.unwrap();
    let key = "contract/twice.bin";
    store.put(key, b"x".to_vec(), None).await.unwrap();
    store.delete(key).await.unwrap();
    store.delete(key).await.unwrap();
    assert_eq!(store.get(key).await.unwrap(), None);
}

#[tokio::test]
async fn presigned_urls_are_shaped() {
    let store = store().await;
    let key = "contract/presigned.bin";
    store.put(key, b"data".to_vec(), None).await.unwrap();
    let get_url = store.presign_get(key, PRESIGN_EXPIRY).await.unwrap();
    assert!(get_url.contains(key), "key in URL: {get_url}");
    assert!(get_url.contains("X-Amz-Signature="), "signed: {get_url}");
    let put_url = store
        .presign_put("contract/presigned-upload.bin", PRESIGN_EXPIRY)
        .await
        .unwrap();
    assert!(put_url.contains("X-Amz-Signature="), "signed: {put_url}");
    store.delete(key).await.unwrap();
}

#[tokio::test]
async fn missing_key_reads_return_none() {
    let store = store().await;
    assert_eq!(store.get("contract/missing-get").await.unwrap(), None);
    assert_eq!(store.head("contract/missing-head").await.unwrap(), None);
}
