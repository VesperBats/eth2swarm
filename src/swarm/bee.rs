//! Minimal client for the Bee HTTP API.

use super::soc::SignedSoc;
use alloy::hex;
use alloy::primitives::{Address, B256};
use alloy::transports::http::reqwest::Url;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::time::Duration;

const RETRIES: u32 = 5;

async fn backoff(attempt: u32) {
    if attempt > 0 {
        tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
    }
}

pub struct Bee {
    base: Url,
    http: reqwest::Client,
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

impl Bee {
    pub fn new(mut base: Url) -> Result<Self> {
        // `Url::join` replaces the last path segment unless the base ends with '/'.
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .context("building HTTP client")?;
        Ok(Self { base, http })
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

    /// Upload arbitrary bytes as a Swarm file (chunk tree); returns its root reference.
    pub async fn upload_bytes(&self, batch: &B256, data: Vec<u8>) -> Result<B256> {
        self.post_with_retry(self.url("bytes")?, batch, data).await
    }

    /// Upload a signed SOC; returns its address as reported by Bee.
    pub async fn upload_soc(&self, batch: &B256, owner: &Address, soc: &SignedSoc) -> Result<B256> {
        let path = format!(
            "soc/{}/{}?sig={}",
            hex::encode(owner),
            hex::encode(soc.identifier),
            hex::encode(soc.signature)
        );
        self.post_with_retry(self.url(&path)?, batch, soc.span_and_payload())
            .await
    }

    async fn post_with_retry(&self, url: Url, batch: &B256, data: Vec<u8>) -> Result<B256> {
        let mut last = None;
        for attempt in 0..RETRIES {
            backoff(attempt).await;
            let sent = self
                .http
                .post(url.clone())
                .header("Swarm-Postage-Batch-Id", hex::encode(batch))
                .header("Swarm-Deferred-Upload", "false")
                .header("Content-Type", "application/octet-stream")
                .body(data.clone())
                .send()
                .await;
            match sent {
                Ok(resp) if resp.status().is_success() => {
                    return Ok(resp.json::<Reference>().await?.reference);
                }
                Ok(resp) if !resp.status().is_server_error() => {
                    return Err(check(resp).await.unwrap_err());
                }
                Ok(resp) => last = Some(check(resp).await.unwrap_err()),
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
        for attempt in 0..RETRIES {
            backoff(attempt).await;
            let mut resp = match self.http.get(url.clone()).send().await {
                Ok(r) => r,
                Err(e) => {
                    last = e.to_string();
                    continue;
                }
            };
            let status = resp.status();
            if status.is_success() {
                let mut body = Vec::new();
                while let Some(part) = resp.chunk().await? {
                    ensure!(
                        body.len() + part.len() <= max_len,
                        "GET {url}: response exceeds {max_len} bytes"
                    );
                    body.extend_from_slice(&part);
                }
                return Ok(body);
            }
            last = format!("{status}: {}", resp.text().await.unwrap_or_default());
            if !(status.as_u16() == 404 || status.is_server_error()) {
                break;
            }
        }
        bail!("GET {url} failed: {last}")
    }
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let url = resp.url().clone();
    let body = resp.text().await.unwrap_or_default();
    bail!("{url} returned {status}: {body}")
}
