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
//!
//! Transient transport errors and retryable 5xx responses (`500`/`502`/`503`
//! `SlowDown`/`504`) are retried with bounded exponential backoff and full
//! jitter. The object-store protocols tolerate ambiguous success, so a retried
//! write is safe; the conditional verbs additionally reconcile against the
//! current bytes after a retry so an early write that committed before its
//! response was lost is reported as success rather than a spurious conflict.

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

/// Transient transport errors and retryable 5xx responses are retried this many
/// times beyond the first attempt, with exponential backoff and full jitter.
const TRANSIENT_RETRIES: u32 = 5;

/// Base backoff before the first transient retry; it doubles each attempt.
const RETRY_BASE: Duration = Duration::from_millis(50);

/// Upper bound on a single transient backoff sleep.
const RETRY_CAP: Duration = Duration::from_secs(2);

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

    /// Send a freshly built request, retrying transient transport failures and
    /// retryable 5xx responses with bounded exponential backoff. `build` is
    /// re-invoked per attempt so each request carries a freshly presigned URL.
    /// The returned flag reports whether any retry occurred, so conditional
    /// writers can reconcile an ambiguous success — a write that may have
    /// committed before a transient failure hid its response.
    async fn send_retrying<F>(&self, build: F) -> Result<(reqwest::Response, bool)>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let mut attempt: u32 = 0;
        let mut retried = false;
        loop {
            match build().send().await {
                Ok(response) => {
                    if is_retryable_status(response.status()) && attempt < TRANSIENT_RETRIES {
                        drop(response);
                        transient_backoff(attempt).await;
                        attempt = attempt.saturating_add(1);
                        retried = true;
                        continue;
                    }
                    return Ok((response, retried));
                }
                Err(error) => {
                    if attempt < TRANSIENT_RETRIES {
                        transient_backoff(attempt).await;
                        attempt = attempt.saturating_add(1);
                        retried = true;
                        continue;
                    }
                    return Err(transport(error));
                }
            }
        }
    }

    async fn head_size(&self, key: &str) -> Result<u64> {
        let full = self.full_key(key);
        let (response, _) = self
            .send_retrying(|| {
                let url = self
                    .bucket
                    .head_object(Some(&self.credentials), &full)
                    .sign(SIGN_TTL);
                self.client.head(url)
            })
            .await?;
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

    /// One conditional PUT, with transient retries already applied. A precondition
    /// failure that follows a retry is reconciled against the current bytes, so an
    /// earlier attempt that committed before its response was lost is reported as
    /// success rather than a spurious conflict.
    async fn conditional_put(
        &self,
        key: &str,
        bytes: &Bytes,
        header: (reqwest::header::HeaderName, String),
    ) -> Result<CondOutcome> {
        let full = self.full_key(key);
        let (response, retried) = self
            .send_retrying(|| {
                let url = self
                    .bucket
                    .put_object(Some(&self.credentials), &full)
                    .sign(SIGN_TTL);
                self.client
                    .put(url)
                    .header(header.0.clone(), header.1.clone())
                    .body(bytes.clone())
            })
            .await?;
        match response.status() {
            StatusCode::OK => Ok(CondOutcome::Set(etag_of(&response, key)?)),
            StatusCode::CONFLICT => Ok(CondOutcome::Conflict),
            StatusCode::PRECONDITION_FAILED | StatusCode::NOT_FOUND => {
                if retried {
                    self.reconcile_conditional(key, bytes).await
                } else {
                    Ok(CondOutcome::PreconditionFailed(None))
                }
            }
            status => Err(unexpected(
                "conditional PUT",
                key,
                status,
                body_excerpt(response).await,
            )),
        }
    }

    /// Re-read a key after an ambiguous conditional write to decide whether our
    /// own bytes won the race. Byte equality — not ETag/content-hash equality —
    /// is the test, because S3 ETags are not Sana content versions.
    async fn reconcile_conditional(&self, key: &str, bytes: &Bytes) -> Result<CondOutcome> {
        match self.get(key).await {
            Ok(current) if current.bytes == *bytes => Ok(CondOutcome::Set(current.version)),
            Ok(current) => Ok(CondOutcome::PreconditionFailed(Some(current.version))),
            Err(Error::NotFound(_)) => Ok(CondOutcome::PreconditionFailed(None)),
            Err(error) => Err(error),
        }
    }
}

/// The outcome of one conditional PUT after transient retries are exhausted.
enum CondOutcome {
    /// The write is durable (or an ambiguous retry was reconciled to success).
    Set(ObjectVersion),
    /// S3 reported a racing conditional write (409); retry the CAS loop.
    Conflict,
    /// The precondition genuinely does not hold; carries the current version
    /// when it could be read back during reconciliation.
    PreconditionFailed(Option<ObjectVersion>),
}

#[async_trait]
impl ObjectStore for S3ObjectStore {
    async fn get(&self, key: &str) -> Result<GetResult> {
        let full = self.full_key(key);
        let (response, _) = self
            .send_retrying(|| {
                let url = self
                    .bucket
                    .get_object(Some(&self.credentials), &full)
                    .sign(SIGN_TTL);
                self.client.get(url)
            })
            .await?;
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
        let (response, _) = self
            .send_retrying(|| {
                let url = self
                    .bucket
                    .get_object(Some(&self.credentials), &full)
                    .sign(SIGN_TTL);
                self.client
                    .get(url)
                    .header(RANGE, format!("bytes={}-{}", range.start, range.end - 1))
            })
            .await?;
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
        let (response, _) = self
            .send_retrying(|| {
                let url = self
                    .bucket
                    .put_object(Some(&self.credentials), &full)
                    .sign(SIGN_TTL);
                self.client.put(url).body(bytes.clone())
            })
            .await?;
        match response.status() {
            StatusCode::OK => etag_of(&response, key),
            status => Err(unexpected("PUT", key, status, body_excerpt(response).await)),
        }
    }

    async fn put_if_absent(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion> {
        for attempt in 0..=CONDITIONAL_CONFLICT_RETRIES {
            match self
                .conditional_put(key, &bytes, (IF_NONE_MATCH, "*".to_string()))
                .await?
            {
                CondOutcome::Set(version) => return Ok(version),
                CondOutcome::Conflict => backoff(attempt).await,
                CondOutcome::PreconditionFailed(_) => {
                    return Err(Error::AlreadyExists(key.to_string()));
                }
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
                .await?
            {
                CondOutcome::Set(version) => return Ok(version),
                CondOutcome::Conflict => backoff(attempt).await,
                CondOutcome::PreconditionFailed(actual) => {
                    return Err(Error::CasMismatch {
                        key: key.to_string(),
                        expected,
                        actual,
                    });
                }
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
            let (response, _) = self
                .send_retrying(|| {
                    let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
                    action.with_prefix(full_prefix.as_str());
                    if let Some(token) = &token {
                        action.with_continuation_token(token.as_str());
                    }
                    let url = action.sign(SIGN_TTL);
                    self.client.get(url)
                })
                .await?;
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
        let (response, _) = self
            .send_retrying(|| {
                let url = self
                    .bucket
                    .delete_object(Some(&self.credentials), &full)
                    .sign(SIGN_TTL);
                self.client.delete(url)
            })
            .await?;
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

/// Whether an S3 status code is a transient server/throttling failure worth
/// retrying. S3 signals throttling with `503 SlowDown`; `500`/`502`/`504` are
/// transient gateway/server faults. Everything else (including `404`, the `409`
/// conditional conflict, and `412` precondition failure) is handled by the
/// per-operation status match and is never retried here.
fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 500 | 502 | 503 | 504)
}

/// Full-jitter exponential backoff: a uniform random duration within
/// `[0, min(RETRY_CAP, RETRY_BASE * 2^attempt)]`. Jitter decorrelates retries
/// across the many pods that share one bucket, so a throttling store does not
/// trigger a synchronized thundering herd. `jitter` is a fraction in `[0, 1]`.
fn backoff_delay(attempt: u32, jitter: f64) -> Duration {
    let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let ceiling = RETRY_BASE.saturating_mul(factor).min(RETRY_CAP);
    ceiling.mul_f64(jitter.clamp(0.0, 1.0))
}

/// A jitter fraction in `[0, 1)` from the system clock's sub-second component.
/// Backoff needs only enough entropy to spread retries across processes, not
/// cryptographic randomness, so a clock read is sufficient and dependency-free.
fn jitter_fraction() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos) * 1e-9
}

async fn transient_backoff(attempt: u32) {
    tokio::time::sleep(backoff_delay(attempt, jitter_fraction())).await;
}

async fn backoff(attempt: u32) {
    tokio::time::sleep(Duration::from_millis(25 * u64::from(attempt + 1))).await;
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )]

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

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

    #[test]
    fn retryable_status_classification() {
        for code in [500u16, 502, 503, 504] {
            assert!(is_retryable_status(StatusCode::from_u16(code).unwrap()));
        }
        for code in [200u16, 204, 206, 404, 409, 412, 501, 505] {
            assert!(!is_retryable_status(StatusCode::from_u16(code).unwrap()));
        }
    }

    #[test]
    fn backoff_is_bounded_and_monotone() {
        // No jitter sleeps for nothing.
        assert_eq!(backoff_delay(0, 0.0), Duration::ZERO);
        // The first attempt's ceiling is the base delay.
        assert!(backoff_delay(0, 1.0) <= RETRY_BASE);
        // It grows with the attempt at fixed jitter...
        assert!(backoff_delay(2, 1.0) >= backoff_delay(1, 1.0));
        // ...and within an attempt with jitter...
        assert!(backoff_delay(3, 0.75) >= backoff_delay(3, 0.25));
        // ...but never exceeds the cap, even far past the doubling range.
        assert!(backoff_delay(40, 1.0) <= RETRY_CAP);
        assert!(backoff_delay(u32::MAX, 1.0) <= RETRY_CAP);
    }

    /// One scripted reply from the mock S3 endpoint. `None` means "read the
    /// request, then drop the connection without replying", which surfaces in
    /// reqwest as a transient transport error.
    type Reply = Option<(u16, Vec<(&'static str, String)>, Vec<u8>)>;

    fn reason_phrase(code: u16) -> &'static str {
        match code {
            200 => "OK",
            204 => "No Content",
            206 => "Partial Content",
            412 => "Precondition Failed",
            503 => "Service Unavailable",
            _ => "Status",
        }
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
    }

    /// Read one HTTP request fully (headers plus any declared body) so the
    /// connection is drained before we reply or close it.
    async fn read_request(socket: &mut TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&chunk[..n]);
            let Some(header_end) = find_header_end(&buf) else {
                continue;
            };
            let header_text = String::from_utf8_lossy(&buf[..header_end]);
            let content_length = header_text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            let mut remaining = content_length.saturating_sub(buf.len() - header_end);
            while remaining > 0 {
                let n = match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                remaining = remaining.saturating_sub(n);
            }
            return;
        }
    }

    /// Spawn a mock S3 endpoint that answers `replies` in order, one request
    /// per connection. Every response sets `Connection: close`, so reqwest opens
    /// a fresh connection per attempt and each scripted reply maps to one accept.
    async fn spawn_mock(replies: Vec<Reply>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                read_request(&mut socket).await;
                match reply {
                    None => {
                        let _ = socket.shutdown().await;
                    }
                    Some((code, headers, body)) => {
                        let mut out = format!("HTTP/1.1 {code} {}\r\n", reason_phrase(code));
                        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
                        for (name, value) in &headers {
                            out.push_str(&format!("{name}: {value}\r\n"));
                        }
                        out.push_str("Connection: close\r\n\r\n");
                        let mut bytes = out.into_bytes();
                        bytes.extend_from_slice(&body);
                        let _ = socket.write_all(&bytes).await;
                        let _ = socket.shutdown().await;
                    }
                }
            }
        });
        (endpoint, handle)
    }

    fn store_for(endpoint: &str) -> S3ObjectStore {
        S3ObjectStore::new(
            S3Config {
                endpoint: endpoint.to_string(),
                region: "us-east-1".into(),
                bucket: "b".into(),
                key_prefix: String::new(),
                path_style: true,
            },
            Credentials::new("ak", "sk"),
        )
        .unwrap()
    }

    fn ok_with_etag(etag: &str, body: &[u8]) -> Reply {
        Some((200, vec![("ETag", etag.to_string())], body.to_vec()))
    }

    fn transient_503() -> Reply {
        Some((503, vec![], b"SlowDown".to_vec()))
    }

    #[tokio::test]
    async fn get_retries_transient_5xx_then_succeeds() {
        let (endpoint, server) = spawn_mock(vec![
            transient_503(),
            transient_503(),
            ok_with_etag("\"v1\"", b"hello"),
        ])
        .await;
        let store = store_for(&endpoint);
        let got = store.get("k").await.unwrap();
        assert_eq!(&got.bytes[..], b"hello");
        assert_eq!(got.version, ObjectVersion("v1".into()));
        server.abort();
    }

    #[tokio::test]
    async fn get_surfaces_error_after_retry_budget() {
        let replies: Vec<Reply> = (0..=TRANSIENT_RETRIES).map(|_| transient_503()).collect();
        let (endpoint, server) = spawn_mock(replies).await;
        let store = store_for(&endpoint);
        let error = store.get("k").await.unwrap_err();
        assert!(matches!(error, Error::Io(_)), "got {error:?}");
        server.abort();
    }

    #[tokio::test]
    async fn put_retries_dropped_connection_then_succeeds() {
        let (endpoint, server) = spawn_mock(vec![None, ok_with_etag("\"v2\"", &[])]).await;
        let store = store_for(&endpoint);
        let version = store.put("k", Bytes::from_static(b"data")).await.unwrap();
        assert_eq!(version, ObjectVersion("v2".into()));
        server.abort();
    }

    #[tokio::test]
    async fn get_range_retries_then_returns_slice() {
        let (endpoint, server) = spawn_mock(vec![
            transient_503(),
            Some((
                206,
                vec![("Content-Range", "bytes 0-2/5".into())],
                b"abc".to_vec(),
            )),
        ])
        .await;
        let store = store_for(&endpoint);
        let bytes = store.get_range("k", 0..3).await.unwrap();
        assert_eq!(&bytes[..], b"abc");
        server.abort();
    }

    #[tokio::test]
    async fn delete_retries_then_succeeds() {
        let (endpoint, server) =
            spawn_mock(vec![transient_503(), Some((204, vec![], vec![]))]).await;
        let store = store_for(&endpoint);
        store.delete("k").await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn list_retries_then_parses() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
<Name>b</Name><Prefix></Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys>\
<IsTruncated>false</IsTruncated>\
<Contents><Key>k</Key><LastModified>2024-01-01T00:00:00.000Z</LastModified>\
<ETag>\"v\"</ETag><Size>3</Size><StorageClass>STANDARD</StorageClass></Contents>\
</ListBucketResult>";
        let (endpoint, server) = spawn_mock(vec![
            transient_503(),
            Some((200, vec![], xml.as_bytes().to_vec())),
        ])
        .await;
        let store = store_for(&endpoint);
        let listed = store.list("").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, "k");
        assert_eq!(listed[0].size, 3);
        server.abort();
    }

    #[tokio::test]
    async fn put_if_absent_reconciles_ambiguous_success() {
        // The first attempt's response is lost after the object is written; the
        // retry sees 412, and the reconciling GET returns our exact bytes.
        let (endpoint, server) = spawn_mock(vec![
            None,
            Some((412, vec![], vec![])),
            ok_with_etag("\"v3\"", b"payload"),
        ])
        .await;
        let store = store_for(&endpoint);
        let version = store
            .put_if_absent("k", Bytes::from_static(b"payload"))
            .await
            .unwrap();
        assert_eq!(version, ObjectVersion("v3".into()));
        server.abort();
    }

    #[tokio::test]
    async fn put_if_absent_reports_real_conflict_without_extra_get() {
        // No transient failure: a direct 412 means the object already exists,
        // and there is no reconciling GET (one request only).
        let (endpoint, server) = spawn_mock(vec![Some((412, vec![], vec![]))]).await;
        let store = store_for(&endpoint);
        let error = store
            .put_if_absent("k", Bytes::from_static(b"payload"))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::AlreadyExists(_)), "got {error:?}");
        server.abort();
    }

    #[tokio::test]
    async fn compare_and_set_reconciles_ambiguous_win() {
        let (endpoint, server) = spawn_mock(vec![
            None,
            Some((412, vec![], vec![])),
            ok_with_etag("\"v5\"", b"new"),
        ])
        .await;
        let store = store_for(&endpoint);
        let version = store
            .compare_and_set("k", ObjectVersion("old".into()), Bytes::from_static(b"new"))
            .await
            .unwrap();
        assert_eq!(version, ObjectVersion("v5".into()));
        server.abort();
    }

    #[tokio::test]
    async fn compare_and_set_reports_mismatch_when_bytes_differ() {
        let (endpoint, server) = spawn_mock(vec![
            None,
            Some((412, vec![], vec![])),
            ok_with_etag("\"other\"", b"someone-elses"),
        ])
        .await;
        let store = store_for(&endpoint);
        let error = store
            .compare_and_set(
                "k",
                ObjectVersion("old".into()),
                Bytes::from_static(b"mine"),
            )
            .await
            .unwrap_err();
        match error {
            Error::CasMismatch { actual, .. } => {
                assert_eq!(actual, Some(ObjectVersion("other".into())));
            }
            other => panic!("expected CasMismatch, got {other:?}"),
        }
        server.abort();
    }
}
