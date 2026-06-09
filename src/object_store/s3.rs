//! S3-compatible object store over presigned HTTP requests.
//!
//! Conditional writes use S3's native preconditions — `If-None-Match: *` for
//! put-if-absent and `If-Match: <etag>` for compare-and-set — so CAS is
//! enforced by the store itself, across processes and nodes. This lifts the
//! filesystem backend's single-process CAS limitation (D4). Object versions
//! wrap S3 ETags: version equality is all CAS needs, and the engine's
//! immutable-object recovery paths compare bytes directly, never ETag against
//! content hash.
//!
//! Requests are presigned with SigV4 query auth and sent with `reqwest`. The
//! conditional headers are standard HTTP (not `x-amz-*`), so they ride
//! unsigned next to the presigned URL.

use std::ops::Range;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, ETAG, IF_MATCH, IF_NONE_MATCH, RANGE};
use rusty_s3::actions::ListObjectsV2;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

use super::{GetResult, ObjectMeta, ObjectStore, ObjectVersion};
use crate::error::{Error, Result};

/// Presigned URLs are minted per request and used immediately; the TTL only
/// needs to cover clock skew plus one request.
const SIGN_TTL: Duration = Duration::from_secs(300);

/// S3 answers 409 `ConditionalRequestConflict` when conditional writes race on
/// one key; the loser should simply try again.
const CONDITIONAL_CONFLICT_RETRIES: u32 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Config {
    /// E.g. `https://s3.us-east-1.amazonaws.com` or `http://127.0.0.1:9000`.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Key prefix inside the bucket; empty for the bucket root.
    pub key_prefix: String,
    /// Path-style URLs (`endpoint/bucket/key`) for MinIO and other
    /// S3-compatible stores; virtual-host style for AWS itself.
    pub path_style: bool,
}

impl S3Config {
    /// Parse `s3://bucket[/prefix]` plus environment configuration:
    /// `SANA_S3_ENDPOINT` (default `https://s3.<region>.amazonaws.com`),
    /// `AWS_REGION` / `AWS_DEFAULT_REGION` (default `us-east-1`), and
    /// `SANA_S3_PATH_STYLE` (default: path-style for custom endpoints,
    /// virtual-host for AWS).
    pub fn from_location(location: &str) -> Result<Self> {
        let rest = location.strip_prefix("s3://").ok_or_else(|| {
            Error::InvalidWrite(format!(
                "S3 location must start with s3://, got {location:?}"
            ))
        })?;
        let (bucket, prefix) = match rest.split_once('/') {
            Some((bucket, prefix)) => (bucket, prefix.trim_matches('/')),
            None => (rest, ""),
        };
        if bucket.is_empty() {
            return Err(Error::InvalidWrite(format!(
                "S3 location {location:?} is missing a bucket name"
            )));
        }
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_string());
        let endpoint = std::env::var("SANA_S3_ENDPOINT")
            .unwrap_or_else(|_| format!("https://s3.{region}.amazonaws.com"));
        let path_style = match std::env::var("SANA_S3_PATH_STYLE") {
            Ok(value) => value == "1" || value.eq_ignore_ascii_case("true"),
            Err(_) => !endpoint.ends_with(".amazonaws.com"),
        };
        Ok(Self {
            endpoint,
            region,
            bucket: bucket.to_string(),
            key_prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            },
            path_style,
        })
    }
}

pub struct S3ObjectStore {
    bucket: Bucket,
    credentials: Credentials,
    client: reqwest::Client,
    key_prefix: String,
}

impl S3ObjectStore {
    /// Build a store with credentials from `AWS_ACCESS_KEY_ID`,
    /// `AWS_SECRET_ACCESS_KEY`, and optionally `AWS_SESSION_TOKEN`.
    pub fn from_env(config: S3Config) -> Result<Self> {
        let credentials = Credentials::from_env().ok_or_else(|| {
            other("AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY are not set".to_string())
        })?;
        Self::new(config, credentials)
    }

    pub fn new(config: S3Config, credentials: Credentials) -> Result<Self> {
        let endpoint: url::Url = config.endpoint.parse().map_err(|error| {
            other(format!(
                "invalid S3 endpoint {:?}: {error}",
                config.endpoint
            ))
        })?;
        let style = if config.path_style {
            UrlStyle::Path
        } else {
            UrlStyle::VirtualHost
        };
        let bucket = Bucket::new(endpoint, style, config.bucket, config.region)
            .map_err(|error| other(format!("invalid S3 bucket config: {error}")))?;
        let client = reqwest::Client::builder()
            .build()
            .map_err(|error| other(format!("building S3 HTTP client: {error}")))?;
        Ok(Self {
            bucket,
            credentials,
            client,
            key_prefix: config.key_prefix,
        })
    }

    fn full_key(&self, key: &str) -> String {
        format!("{}{key}", self.key_prefix)
    }

    async fn head_size(&self, key: &str) -> Result<u64> {
        let full = self.full_key(key);
        let url = self
            .bucket
            .head_object(Some(&self.credentials), &full)
            .sign(SIGN_TTL);
        let response = self.client.head(url).send().await.map_err(transport)?;
        match response.status() {
            StatusCode::OK => response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| other(format!("S3 HEAD for {key:?} returned no content length"))),
            StatusCode::NOT_FOUND => Err(Error::NotFound(key.to_string())),
            status => Err(unexpected("HEAD", key, status, String::new())),
        }
    }

    /// One conditional PUT attempt. Returns `None` when S3 reports a racing
    /// conditional write (409), which the caller retries.
    async fn conditional_put(
        &self,
        key: &str,
        bytes: &Bytes,
        header: (reqwest::header::HeaderName, String),
    ) -> Result<Option<ObjectVersion>> {
        let full = self.full_key(key);
        let url = self
            .bucket
            .put_object(Some(&self.credentials), &full)
            .sign(SIGN_TTL);
        let response = self
            .client
            .put(url)
            .header(header.0, header.1)
            .body(bytes.clone())
            .send()
            .await
            .map_err(transport)?;
        match response.status() {
            StatusCode::OK => Ok(Some(etag_of(&response, key)?)),
            StatusCode::CONFLICT => Ok(None),
            StatusCode::PRECONDITION_FAILED | StatusCode::NOT_FOUND => {
                Err(Error::NotFound(String::new())) // caller maps per operation
            }
            status => Err(unexpected(
                "conditional PUT",
                key,
                status,
                body_excerpt(response).await,
            )),
        }
    }
}

#[async_trait]
impl ObjectStore for S3ObjectStore {
    async fn get(&self, key: &str) -> Result<GetResult> {
        let full = self.full_key(key);
        let url = self
            .bucket
            .get_object(Some(&self.credentials), &full)
            .sign(SIGN_TTL);
        let response = self.client.get(url).send().await.map_err(transport)?;
        match response.status() {
            StatusCode::OK => {
                let version = etag_of(&response, key)?;
                let bytes = response.bytes().await.map_err(transport)?;
                Ok(GetResult { bytes, version })
            }
            StatusCode::NOT_FOUND => Err(Error::NotFound(key.to_string())),
            status => Err(unexpected("GET", key, status, body_excerpt(response).await)),
        }
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
        if range.start > range.end {
            let size = self.head_size(key).await?;
            return Err(Error::InvalidRange {
                start: range.start,
                end: range.end,
                size,
            });
        }
        if range.start == range.end {
            // HTTP cannot express an empty range; match the filesystem
            // semantics (bounds-checked empty read) with a HEAD.
            let size = self.head_size(key).await?;
            if range.end > size {
                return Err(Error::InvalidRange {
                    start: range.start,
                    end: range.end,
                    size,
                });
            }
            return Ok(Bytes::new());
        }

        let full = self.full_key(key);
        let url = self
            .bucket
            .get_object(Some(&self.credentials), &full)
            .sign(SIGN_TTL);
        let response = self
            .client
            .get(url)
            .header(RANGE, format!("bytes={}-{}", range.start, range.end - 1))
            .send()
            .await
            .map_err(transport)?;
        match response.status() {
            StatusCode::PARTIAL_CONTENT => {
                // S3 clamps a range that runs past the object; the trait
                // demands an exact-bounds error instead. `Content-Range`
                // carries the total size: `bytes start-end/size`.
                let size = content_range_size(&response);
                let bytes = response.bytes().await.map_err(transport)?;
                if bytes.len() as u64 != range.end - range.start {
                    return Err(Error::InvalidRange {
                        start: range.start,
                        end: range.end,
                        size: size.unwrap_or(bytes.len() as u64),
                    });
                }
                Ok(bytes)
            }
            StatusCode::RANGE_NOT_SATISFIABLE => {
                let size = self.head_size(key).await?;
                Err(Error::InvalidRange {
                    start: range.start,
                    end: range.end,
                    size,
                })
            }
            StatusCode::NOT_FOUND => Err(Error::NotFound(key.to_string())),
            status => Err(unexpected(
                "ranged GET",
                key,
                status,
                body_excerpt(response).await,
            )),
        }
    }

    async fn put(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion> {
        let full = self.full_key(key);
        let url = self
            .bucket
            .put_object(Some(&self.credentials), &full)
            .sign(SIGN_TTL);
        let response = self
            .client
            .put(url)
            .body(bytes)
            .send()
            .await
            .map_err(transport)?;
        match response.status() {
            StatusCode::OK => etag_of(&response, key),
            status => Err(unexpected("PUT", key, status, body_excerpt(response).await)),
        }
    }

    async fn put_if_absent(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion> {
        for attempt in 0..=CONDITIONAL_CONFLICT_RETRIES {
            match self
                .conditional_put(key, &bytes, (IF_NONE_MATCH, "*".to_string()))
                .await
            {
                Ok(Some(version)) => return Ok(version),
                Ok(None) => backoff(attempt).await,
                Err(Error::NotFound(_)) => return Err(Error::AlreadyExists(key.to_string())),
                Err(error) => return Err(error),
            }
        }
        Err(other(format!(
            "S3 conditional-write conflict persisted for {key:?}"
        )))
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: ObjectVersion,
        bytes: Bytes,
    ) -> Result<ObjectVersion> {
        // ETags travel quoted on the wire. A token from another backend (a
        // content hash) simply never matches, surfacing as CasMismatch.
        let if_match = format!("\"{}\"", expected.0);
        for attempt in 0..=CONDITIONAL_CONFLICT_RETRIES {
            match self
                .conditional_put(key, &bytes, (IF_MATCH, if_match.clone()))
                .await
            {
                Ok(Some(version)) => return Ok(version),
                Ok(None) => backoff(attempt).await,
                Err(Error::NotFound(_)) => {
                    return Err(Error::CasMismatch {
                        key: key.to_string(),
                        expected,
                        actual: None,
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Err(other(format!(
            "S3 conditional-write conflict persisted for {key:?}"
        )))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let full_prefix = self.full_key(prefix);
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
            action.with_prefix(full_prefix.as_str());
            if let Some(token) = &token {
                action.with_continuation_token(token.as_str());
            }
            let url = action.sign(SIGN_TTL);
            let response = self.client.get(url).send().await.map_err(transport)?;
            if response.status() != StatusCode::OK {
                let status = response.status();
                return Err(unexpected(
                    "LIST",
                    prefix,
                    status,
                    body_excerpt(response).await,
                ));
            }
            let text = response.text().await.map_err(transport)?;
            let parsed = ListObjectsV2::parse_response(&text)
                .map_err(|error| Error::Corrupt(format!("S3 list response: {error}")))?;
            for content in parsed.contents {
                let key = content
                    .key
                    .strip_prefix(&self.key_prefix)
                    .unwrap_or(&content.key)
                    .to_string();
                out.push(ObjectMeta {
                    key,
                    size: content.size,
                    version: etag_version(&content.etag),
                });
            }
            match parsed.next_continuation_token {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        Ok(out)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let full = self.full_key(key);
        let url = self
            .bucket
            .delete_object(Some(&self.credentials), &full)
            .sign(SIGN_TTL);
        let response = self.client.delete(url).send().await.map_err(transport)?;
        match response.status() {
            StatusCode::NO_CONTENT | StatusCode::OK | StatusCode::NOT_FOUND => Ok(()),
            status => Err(unexpected(
                "DELETE",
                key,
                status,
                body_excerpt(response).await,
            )),
        }
    }
}

fn etag_version(raw: &str) -> ObjectVersion {
    ObjectVersion(raw.trim_matches('"').to_string())
}

fn etag_of(response: &reqwest::Response, key: &str) -> Result<ObjectVersion> {
    response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(etag_version)
        .ok_or_else(|| other(format!("S3 response for {key:?} carried no ETag")))
}

fn content_range_size(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit_once('/'))
        .and_then(|(_, size)| size.parse().ok())
}

async fn body_excerpt(response: reqwest::Response) -> String {
    match response.text().await {
        Ok(text) => text.chars().take(256).collect(),
        Err(_) => String::new(),
    }
}

fn transport(error: reqwest::Error) -> Error {
    other(format!("S3 transport: {error}"))
}

fn unexpected(op: &str, key: &str, status: StatusCode, body: String) -> Error {
    other(format!("S3 {op} for {key:?} failed with {status}: {body}"))
}

fn other(message: String) -> Error {
    Error::Io(std::io::Error::other(message))
}

async fn backoff(attempt: u32) {
    tokio::time::sleep(Duration::from_millis(25 * u64::from(attempt + 1))).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_parses_bucket_and_prefix() {
        // Env-dependent fields (endpoint/region/path_style) are intentionally
        // not asserted here; only the location grammar is.
        let config = S3Config::from_location("s3://my-bucket").unwrap();
        assert_eq!(config.bucket, "my-bucket");
        assert_eq!(config.key_prefix, "");

        let config = S3Config::from_location("s3://my-bucket/tenants/a/").unwrap();
        assert_eq!(config.bucket, "my-bucket");
        assert_eq!(config.key_prefix, "tenants/a/");

        assert!(S3Config::from_location("s3://").is_err());
        assert!(S3Config::from_location("/local/dir").is_err());
    }

    #[test]
    fn keys_are_prefixed_and_etags_unquoted() {
        let store = S3ObjectStore::new(
            S3Config {
                endpoint: "http://127.0.0.1:9000".into(),
                region: "us-east-1".into(),
                bucket: "sana".into(),
                key_prefix: "tenants/a/".into(),
                path_style: true,
            },
            Credentials::new("ak", "sk"),
        )
        .unwrap();
        assert_eq!(
            store.full_key("namespaces/docs/manifest/current"),
            "tenants/a/namespaces/docs/manifest/current"
        );
        assert_eq!(
            etag_version("\"abc123\""),
            ObjectVersion("abc123".to_string())
        );
    }
}
