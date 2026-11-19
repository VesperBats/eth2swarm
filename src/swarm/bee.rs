//! Minimal client for the Bee HTTP API.

use super::soc::SignedSoc;
use alloy::hex;
use alloy::primitives::{Address, B256};
use alloy::transports::http::reqwest::Url;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::time::Duration;

const RETRIES: u32 = 5;
/// How much of an error response's body is kept for the message.
const ERROR_BODY_MAX: usize = 1024;
/// Upper bound for an upload's JSON answer (`{"reference": "…"}`).
const UPLOAD_ANSWER_MAX: usize = 64 * 1024;

async fn backoff(attempt: u32) {
    if attempt > 0 {
        tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
    }
}

#[derive(Clone)]
pub struct Bee {
    base: Url,
    http: reqwest::Client,
    retries: u32,
}

#[derive(Deserialize)]
struct Reference {
    reference: B256,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stamp {
    #[serde(rename = "batchID")]
    batch_id: B256,
    usable: bool,
}

#[derive(Deserialize)]
struct Stamps {
    stamps: Vec<Stamp>,
}

/// What Bee says about one postage batch.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchInfo {
    pub usable: bool,
    /// Seconds until the batch expires at the current price (`-1`: unknown).
    #[serde(rename = "batchTTL")]
    pub ttl: i64,
}

impl Bee {
    pub fn new(mut base: Url) -> Result<Self> {
        // `Url::join` replaces the last path segment unless the base ends with '/'.
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Per read, not per request: a large body arriving slowly but steadily
            // completes, and only a connection that stalls for this long fails (and is
            // retried). It also bounds the wait for an upload's answer.
            .read_timeout(Duration::from_secs(120))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            base,
            http,
            retries: RETRIES,
        })
    }

    /// Number of attempts for reads (default 5). Interactive tools use 1 to fail fast.
    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = retries.max(1);
        self
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.base
            .join(path)
            .with_context(|| format!("building Bee URL for {path}"))
    }

    /// First usable postage batch owned by this node.
    pub async fn usable_batch(&self) -> Result<B256> {
        let stamps: Stamps = self
            .http
            .get(self.url("stamps")?)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("parsing /stamps")?;
        stamps
            .stamps
            .into_iter()
            .find(|s| s.usable)
            .map(|s| s.batch_id)
            .context("Bee node has no usable postage batch (pass --batch)")
    }

    /// A postage batch of this node: whether it is usable and how long it lives; `None` if
    /// the node does not know it (never bought here, or expired and gone). Transport errors
    /// and server errors are retried.
    pub async fn batch(&self, id: &B256) -> Result<Option<BatchInfo>> {
        let url = self.url(&format!("stamps/{}", hex::encode(id)))?;
        let mut last = None;
        for attempt in 0..self.retries {
            backoff(attempt).await;
            match self.http.get(url.clone()).send().await {
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => return Ok(None),
                Ok(resp) if resp.status().is_success() => {
                    return resp
                        .json()
                        .await
                        .map(Some)
                        .context("parsing /stamps/{batch}");
                }
                Ok(resp) if !resp.status().is_server_error() => return Err(error_from(resp).await),
                Ok(resp) => last = Some(error_from(resp).await),
                Err(e) => last = Some(e.into()),
            }
        }
        Err(last.unwrap()).with_context(|| format!("GET {url} failed"))
    }

    /// Upload arbitrary bytes as a Swarm file (chunk tree); returns its root reference.
    pub async fn upload_bytes(&self, batch: &B256, data: Vec<u8>) -> Result<B256> {
        self.post_with_retry(self.url("bytes")?, batch, data, &[])
            .await
    }

    /// Upload bytes as a Swarm file without erasure coding (`Swarm-Redundancy-Level: 0`), so
    /// that the reference is the plain chunk tree's (`swarm::file::file_reference`). Bee
    /// 2.8.3 erasure-codes `/bytes` uploads unless told otherwise.
    pub async fn upload_file(&self, batch: &B256, data: Vec<u8>) -> Result<B256> {
        self.post_with_retry(
            self.url("bytes")?,
            batch,
            data,
            &[("Swarm-Redundancy-Level", "0")],
        )
        .await
    }

    /// Upload one content-addressed chunk in wire form (`span ‖ payload`); returns its address
    /// as reported by Bee.
    pub async fn upload_chunk(&self, batch: &B256, wire: Vec<u8>) -> Result<B256> {
        self.post_with_retry(self.url("chunks")?, batch, wire, &[])
            .await
    }

    /// Upload a signed SOC; returns its address as reported by Bee.
    pub async fn upload_soc(&self, batch: &B256, owner: &Address, soc: &SignedSoc) -> Result<B256> {
        let path = format!(
            "soc/{}/{}?sig={}",
            hex::encode(owner),
            hex::encode(soc.identifier),
            hex::encode(soc.signature)
        );
        self.post_with_retry(self.url(&path)?, batch, soc.span_and_payload(), &[])
            .await
    }

    async fn post_with_retry(
        &self,
        url: Url,
        batch: &B256,
        data: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> Result<B256> {
        let mut last = None;
        for attempt in 0..RETRIES {
            backoff(attempt).await;
            let mut req = self
                .http
                .post(url.clone())
                .header("Swarm-Postage-Batch-Id", hex::encode(batch))
                .header("Swarm-Deferred-Upload", "false")
                .header("Content-Type", "application/octet-stream");
            for (name, value) in headers {
                req = req.header(*name, *value);
            }
            let sent = req.body(data.clone()).send().await;
            match sent {
                Ok(resp) if resp.status().is_success() => {
                    // The upload may already be stored; resending the same payload is
                    // harmless (content-addressed, or the same signed SOC), so a failure
                    // to read or parse the answer is retried like a transport error.
                    match read_capped(resp, UPLOAD_ANSWER_MAX).await {
                        Ok(body) => match serde_json::from_slice::<Reference>(&body) {
                            Ok(r) => return Ok(r.reference),
                            Err(e) => last = Some(anyhow::anyhow!("parsing upload answer: {e}")),
                        },
                        Err(e) => last = Some(e.into()),
                    }
                }
                Ok(resp) if !resp.status().is_server_error() => return Err(error_from(resp).await),
                Ok(resp) => last = Some(error_from(resp).await),
                Err(e) => last = Some(e.into()),
            }
        }
        Err(last.unwrap()).with_context(|| format!("POST {url} failed after {RETRIES} attempts"))
    }

    /// Raw chunk data at `address`. Retries briefly, since a freshly uploaded chunk may still
    /// be propagating to the node we read from.
    pub async fn get_chunk(&self, address: &B256) -> Result<Vec<u8>> {
        self.get_with_retry(&format!("chunks/{}", hex::encode(address)), 4096 + 105)
            .await
    }

    /// Download a Swarm file by root reference, refusing files larger than `max_len` bytes.
    pub async fn get_bytes(&self, reference: &B256, max_len: usize) -> Result<Vec<u8>> {
        self.get_with_retry(&format!("bytes/{}", hex::encode(reference)), max_len)
            .await
    }

    async fn get_with_retry(&self, path: &str, max_len: usize) -> Result<Vec<u8>> {
        let url = self.url(path)?;
        let mut last = String::new();
        for attempt in 0..self.retries {
            backoff(attempt).await;
            let resp = match self.http.get(url.clone()).send().await {
                Ok(r) => r,
                Err(e) => {
                    last = format!("{:#}", anyhow::Error::from(e));
                    continue;
                }
            };
            let status = resp.status();
            if status.is_success() {
                match read_capped(resp, max_len).await {
                    Ok(body) => return Ok(body),
                    // Oversized is the content's fault: retrying cannot help.
                    Err(ReadError::TooLarge(max)) => {
                        bail!("GET {url}: response exceeds {max} bytes")
                    }
                    // A reset or timeout mid-body: drop the partial body and retry.
                    Err(e @ ReadError::Transport(_)) => {
                        last = format!("{:#}", anyhow::Error::from(e));
                        continue;
                    }
                }
            }
            last = format!("{status}: {}", error_body(resp).await);
            if !(status.as_u16() == 404 || status.is_server_error()) {
                break;
            }
        }
        bail!("GET {url} failed: {last}")
    }
}

#[derive(Debug)]
enum ReadError {
    TooLarge(usize),
    Transport(reqwest::Error),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::TooLarge(max) => write!(f, "response exceeds {max} bytes"),
            ReadError::Transport(_) => write!(f, "reading response"),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReadError::TooLarge(_) => None,
            ReadError::Transport(e) => Some(e),
        }
    }
}

/// Read a response body, refusing more than `max` bytes.
async fn read_capped(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>, ReadError> {
    let mut body = Vec::new();
    while let Some(part) = resp.chunk().await.map_err(ReadError::Transport)? {
        if body.len() + part.len() > max {
            return Err(ReadError::TooLarge(max));
        }
        body.extend_from_slice(&part);
    }
    Ok(body)
}

/// At most [`ERROR_BODY_MAX`] bytes of an error response's body, for a message; the rest is
/// never read (dropping the response closes the connection).
async fn error_body(mut resp: reqwest::Response) -> String {
    let mut body = Vec::new();
    while body.len() < ERROR_BODY_MAX {
        match resp.chunk().await {
            Ok(Some(part)) => {
                let take = part.len().min(ERROR_BODY_MAX - body.len());
                body.extend_from_slice(&part[..take]);
            }
            _ => break,
        }
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// The error for a non-success response, with a bounded excerpt of its body.
async fn error_from(resp: reqwest::Response) -> anyhow::Error {
    let (url, status) = (resp.url().clone(), resp.status());
    anyhow::anyhow!("{url} returned {status}: {}", error_body(resp).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A local HTTP server answering its n-th connection with `responses[n]` (raw bytes),
    /// then closing it — mid-body when the body is shorter than its Content-Length.
    fn serve(responses: Vec<Vec<u8>>) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(&response);
            }
        });
        url
    }

    fn http(status: &str, content_length: usize, body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    #[tokio::test]
    async fn error_body_is_bounded() {
        let huge = vec![b'x'; 1 << 20];
        let bee = Bee::new(serve(vec![http("400 Bad Request", huge.len(), &huge)])).unwrap();
        let err = bee
            .get_bytes(&B256::ZERO, 16)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("400"), "{err}");
        assert!(
            err.len() < ERROR_BODY_MAX + 300,
            "message is {} bytes",
            err.len()
        );
    }

    #[tokio::test]
    async fn download_cut_mid_body_is_retried() {
        // The first answer promises 10 bytes and closes after 3; the retry gets all of them.
        let bee = Bee::new(serve(vec![
            http("200 OK", 10, b"abc"),
            http("200 OK", 10, b"0123456789"),
        ]))
        .unwrap();
        assert_eq!(bee.get_bytes(&B256::ZERO, 64).await.unwrap(), b"0123456789");
    }

    #[tokio::test]
    async fn retry_error_keeps_its_cause() {
        // Every answer is cut mid-body: the final error must say more than reqwest's top line.
        let cut = || http("200 OK", 10, b"abc");
        let bee = Bee::new(serve((0..RETRIES).map(|_| cut()).collect())).unwrap();
        let err = bee
            .get_bytes(&B256::ZERO, 64)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("reading response: "), "{err}");
        assert!(err.matches(": ").count() >= 3, "no cause chain in: {err}");
    }

    #[tokio::test]
    async fn oversized_download_fails_without_retry() {
        // One answer only: a retry would find no server and fail with another error.
        let bee = Bee::new(serve(vec![http("200 OK", 100, &[0u8; 100])])).unwrap();
        let err = bee
            .get_bytes(&B256::ZERO, 10)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds 10 bytes"), "{err}");
    }

    #[tokio::test]
    async fn upload_answer_cut_mid_body_is_retried() {
        let answer = format!(
            "{{\"reference\":\"{}\"}}",
            hex::encode(B256::repeat_byte(7))
        );
        let bee = Bee::new(serve(vec![
            http("201 Created", answer.len(), &answer.as_bytes()[..5]),
            http("201 Created", answer.len(), answer.as_bytes()),
        ]))
        .unwrap();
        let got = bee
            .upload_bytes(&B256::ZERO, b"data".to_vec())
            .await
            .unwrap();
        assert_eq!(got, B256::repeat_byte(7));
    }
}
