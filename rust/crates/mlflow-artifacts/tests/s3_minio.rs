//! MinIO-gated S3 repository integration coverage.
//!
//! Run with (the bucket must already exist):
//! `MLFLOW_TEST_S3_ENDPOINT=http://127.0.0.1:59090 MLFLOW_TEST_S3_BUCKET=mlflow-soak \
//!  MLFLOW_S3_ENDPOINT_URL=http://127.0.0.1:59090 AWS_ACCESS_KEY_ID=minioadmin \
//!  AWS_SECRET_ACCESS_KEY=minioadmin AWS_REGION=us-east-1 \
//!  cargo test -p mlflow-artifacts --features aws --test s3_minio`

use bytes::Bytes;
use chrono::Utc;
use futures::StreamExt;
use hmac::{Hmac, Mac};
use mlflow_artifacts::repo::MultipartUploadPart;
use mlflow_error::MlflowError;
use reqwest::{Method, Url};
use sha2::{Digest, Sha256};

fn body(chunks: Vec<Bytes>) -> futures::stream::BoxStream<'static, Result<Bytes, MlflowError>> {
    futures::stream::iter(chunks.into_iter().map(Ok)).boxed()
}

async fn download(repo: &dyn mlflow_artifacts::ArtifactRepo, path: &str) -> Vec<u8> {
    let mut stream = repo.get(path).await.unwrap().stream;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    bytes
}

fn minio_uri(test: &str) -> Option<String> {
    std::env::var("MLFLOW_TEST_S3_ENDPOINT").ok()?;
    let bucket =
        std::env::var("MLFLOW_TEST_S3_BUCKET").unwrap_or_else(|_| "mlflow-soak".to_string());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Some(format!(
        "s3://{bucket}/t22-0/{test}-{}-{nonce}",
        std::process::id()
    ))
}

// The object_store Path type strips trailing slashes, so use raw signed S3 PUTs
// to create directory markers and exact key layouts for integration coverage.
async fn put_object(uri: &str, path: &str, contents: &'static [u8]) {
    let response = object_request(Method::PUT, uri, path, contents).await;
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
}

async fn object_exists(uri: &str, path: &str) -> bool {
    object_request(Method::HEAD, uri, path, b"")
        .await
        .status()
        .is_success()
}

async fn object_request(
    method: Method,
    uri: &str,
    path: &str,
    contents: &'static [u8],
) -> reqwest::Response {
    let endpoint = std::env::var("MLFLOW_TEST_S3_ENDPOINT").unwrap();
    let (bucket, root) = uri.strip_prefix("s3://").unwrap().split_once('/').unwrap();
    let url = Url::parse(&format!(
        "{endpoint}/{bucket}/{root}/{}",
        path.trim_start_matches('/')
    ))
    .unwrap();
    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());
    let access_key =
        std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "minioadmin".to_string());
    let secret_key =
        std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".to_string());
    let now = Utc::now();
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let payload_hash = format!("{:x}", Sha256::digest(contents));
    let host = url
        .as_str()
        .strip_prefix(&format!("{}://", url.scheme()))
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "{}\n{}\n\n{}\n{}\n{}",
        method.as_str(),
        url.path(),
        canonical_headers,
        signed_headers,
        payload_hash
    );
    let scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{:x}",
        Sha256::digest(canonical_request.as_bytes())
    );
    let date_key = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let region_key = hmac_sha256(&date_key, region.as_bytes());
    let service_key = hmac_sha256(&region_key, b"s3");
    let signing_key = hmac_sha256(&service_key, b"aws4_request");
    let signature = hmac_sha256(&signing_key, string_to_sign.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    );

    let response = reqwest::Client::new()
        .request(method, url)
        .header("x-amz-content-sha256", payload_hash)
        .header("x-amz-date", amz_date)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .body(contents)
        .send()
        .await
        .unwrap();
    response
}

fn hmac_sha256(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(value);
    mac.finalize().into_bytes().to_vec()
}

#[tokio::test]
async fn put_get_list_delete_roundtrip() {
    let Some(uri) = minio_uri("roundtrip") else {
        eprintln!("skipped: MLFLOW_TEST_S3_ENDPOINT is unset");
        return;
    };
    let repo = mlflow_artifacts::factory::repo_from_uri(&uri).unwrap();
    repo.put(
        "dir/payload.bin",
        body(vec![
            Bytes::from_static(b"part-a"),
            Bytes::from_static(b"part-b"),
        ]),
    )
    .await
    .unwrap();
    assert_eq!(
        download(repo.as_ref(), "dir/payload.bin").await,
        b"part-apart-b"
    );
    let root = repo.list(None).await.unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].path, "dir");
    assert!(root[0].is_dir);
    let nested = repo.list(Some("dir")).await.unwrap();
    assert_eq!(nested.len(), 1);
    assert_eq!(nested[0].path, "dir/payload.bin");
    assert_eq!(nested[0].file_size, Some(12));
    let presigned = repo
        .get_download_presigned_url("dir/payload.bin", 300)
        .await
        .unwrap();
    assert_eq!(presigned.file_size, Some(12));
    assert!(presigned.headers.is_empty());
    let direct = reqwest::get(&presigned.url).await.unwrap();
    assert!(direct.status().is_success());
    assert_eq!(direct.bytes().await.unwrap().as_ref(), b"part-apart-b");
    repo.delete("dir").await.unwrap();
    assert!(repo.list(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn delete_preserves_prefix_siblings_and_trailing_slash_exact_object() {
    let Some(uri) = minio_uri("delete-boundary") else {
        eprintln!("skipped: MLFLOW_TEST_S3_ENDPOINT is unset");
        return;
    };
    let repo = mlflow_artifacts::factory::repo_from_uri(&uri).unwrap();
    put_object(&uri, "foo/a", b"child").await;
    put_object(&uri, "foobar/keep", b"sibling").await;
    put_object(&uri, "foo_baz", b"sibling").await;

    assert_eq!(repo.list(Some("foo")).await.unwrap()[0].path, "foo/a");
    repo.delete("foo").await.unwrap();
    assert!(repo.get("foo/a").await.is_err());
    assert_eq!(download(repo.as_ref(), "foobar/keep").await, b"sibling");
    assert_eq!(download(repo.as_ref(), "foo_baz").await, b"sibling");

    put_object(&uri, "pure-prefix", b"exact").await;
    repo.delete("pure-prefix/").await.unwrap();
    assert_eq!(download(repo.as_ref(), "pure-prefix").await, b"exact");

    repo.delete("").await.unwrap();
}

#[tokio::test]
async fn list_ignores_directory_markers_without_changing_files_or_order() {
    let Some(uri) = minio_uri("directory-marker") else {
        eprintln!("skipped: MLFLOW_TEST_S3_ENDPOINT is unset");
        return;
    };
    let repo = mlflow_artifacts::factory::repo_from_uri(&uri).unwrap();
    put_object(&uri, "b/", b"").await;
    put_object(&uri, "b/b", b"").await;
    repo.put("b/c.txt", body(vec![Bytes::from(vec![b'c'; 42])]))
        .await
        .unwrap();
    repo.put("b/d/child", body(vec![Bytes::from_static(b"d")]))
        .await
        .unwrap();

    let listed = repo.list(Some("b")).await.unwrap();
    let actual = listed
        .iter()
        .map(|file| (file.path.as_str(), file.is_dir, file.file_size))
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        vec![
            ("b/b", false, Some(0)),
            ("b/c.txt", false, Some(42)),
            ("b/d", true, None),
        ]
    );

    repo.delete("").await.unwrap();
    assert!(!object_exists(&uri, "b/").await);
}

#[tokio::test]
async fn multipart_complete_and_abort() {
    let Some(uri) = minio_uri("multipart") else {
        eprintln!("skipped: MLFLOW_TEST_S3_ENDPOINT is unset");
        return;
    };
    let repo = mlflow_artifacts::factory::repo_from_uri(&uri).unwrap();
    let created = repo
        .create_multipart_upload("complete.bin", 2)
        .await
        .unwrap();
    assert_eq!(created.credentials.len(), 2);
    let client = reqwest::Client::new();
    let payloads = [vec![b'a'; 5 * 1024 * 1024], b"tail".to_vec()];
    let mut parts = Vec::new();
    for (credential, payload) in created.credentials.iter().zip(payloads.iter()) {
        let response = client
            .put(&credential.url)
            .body(payload.clone())
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        parts.push(MultipartUploadPart {
            part_number: credential.part_number,
            etag,
            url: credential.url.clone(),
        });
    }
    repo.complete_multipart_upload("complete.bin", &created.upload_id, &parts)
        .await
        .unwrap();
    let completed = download(repo.as_ref(), "complete.bin").await;
    assert_eq!(completed.len(), 5 * 1024 * 1024 + 4);
    assert!(completed[..5 * 1024 * 1024]
        .iter()
        .all(|byte| *byte == b'a'));
    assert_eq!(&completed[5 * 1024 * 1024..], b"tail");

    let aborted = repo
        .create_multipart_upload("aborted.bin", 1)
        .await
        .unwrap();
    let response = client
        .put(&aborted.credentials[0].url)
        .body("discard me")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    repo.abort_multipart_upload("aborted.bin", &aborted.upload_id)
        .await
        .unwrap();
    assert!(repo.get("aborted.bin").await.is_err());

    repo.delete("complete.bin").await.unwrap();
}
