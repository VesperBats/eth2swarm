//! Swarm Single Owner Chunks (SOC).
//!
//! * address   = `keccak256(identifier ‖ owner)`
//! * signature = EIP-191 personal-sign over `keccak256(identifier ‖ cac_address)`
//! * wire form = `identifier(32) ‖ signature(65) ‖ span(8, LE) ‖ payload(≤4096)`

use super::bmt::{CHUNK_SIZE, cac_address};
use alloy::primitives::{Address, B256, Signature, keccak256};
use alloy::signers::{SignerSync, local::PrivateKeySigner};
use anyhow::{Context, Result, bail, ensure};

const ID_LEN: usize = 32;
const SIG_LEN: usize = 65;
const SPAN_LEN: usize = 8;

/// Address of the SOC with `identifier` owned by `owner`.
pub fn soc_address(identifier: &B256, owner: &Address) -> B256 {
    let mut buf = [0u8; 32 + 20];
    buf[..32].copy_from_slice(identifier.as_slice());
    buf[32..].copy_from_slice(owner.as_slice());
    keccak256(buf)
}

fn signing_digest(identifier: &B256, cac: &B256) -> B256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(identifier.as_slice());
    buf[32..].copy_from_slice(cac.as_slice());
    keccak256(buf)
}

/// A signed SOC ready for upload.
pub struct SignedSoc {
    pub identifier: B256,
    pub signature: [u8; 65],
    pub payload: Vec<u8>,
}

impl SignedSoc {
    /// `span ‖ payload` — the request body expected by Bee's `POST /soc/{owner}/{id}`.
    pub fn span_and_payload(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SPAN_LEN + self.payload.len());
        out.extend_from_slice(&(self.payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Sign `payload` as the SOC `identifier` of `signer`.
pub fn sign(signer: &PrivateKeySigner, identifier: B256, payload: Vec<u8>) -> Result<SignedSoc> {
    ensure!(
        payload.len() <= CHUNK_SIZE,
        "SOC payload is {} bytes, max is {CHUNK_SIZE}",
        payload.len()
    );
    let cac = cac_address(payload.len() as u64, &payload);
    let sig = signer
        .sign_message_sync(signing_digest(&identifier, &cac).as_slice())
        .context("signing SOC")?;
    Ok(SignedSoc {
        identifier,
        signature: sig.as_bytes(),
        payload,
    })
}

/// A raw SOC split into its fields, with the signer recovered but nothing checked yet.
pub struct ParsedSoc {
    pub identifier: B256,
    pub signature: [u8; 65],
    pub span: u64,
    pub payload: Vec<u8>,
    /// Address of the inner content-addressed chunk (what the owner signed).
    pub cac_address: B256,
    /// Owner recovered from the signature (`None` if the signature is malformed).
    pub signer: Option<Address>,
}

/// Split a raw SOC as returned by `GET /chunks/{address}` into its fields.
pub fn parse(raw: &[u8]) -> Result<ParsedSoc> {
    ensure!(
        raw.len() >= ID_LEN + SIG_LEN + SPAN_LEN,
        "chunk too short for a SOC ({} bytes)",
        raw.len()
    );
    let identifier = B256::from_slice(&raw[..ID_LEN]);
    let signature: [u8; 65] = raw[ID_LEN..ID_LEN + SIG_LEN].try_into()?;
    let span_start = ID_LEN + SIG_LEN;
    let span = u64::from_le_bytes(raw[span_start..span_start + SPAN_LEN].try_into()?);
    let payload = raw[span_start + SPAN_LEN..].to_vec();
    ensure!(
        payload.len() <= CHUNK_SIZE && span == payload.len() as u64,
        "SOC wraps a multi-chunk or malformed CAC (span {span}, payload {})",
        payload.len()
    );
    let cac_address = cac_address(span, &payload);
    let signer = Signature::from_raw(&signature).ok().and_then(|sig| {
        sig.recover_address_from_msg(signing_digest(&identifier, &cac_address).as_slice())
            .ok()
    });
    Ok(ParsedSoc {
        identifier,
        signature,
        span,
        payload,
        cac_address,
        signer,
    })
}

/// Parse a raw SOC and check that it is a valid SOC at `address` signed by `owner`.
/// Returns the verified payload.
pub fn verify(raw: &[u8], address: &B256, owner: &Address) -> Result<Vec<u8>> {
    let soc = parse(raw)?;
    match soc.signer {
        Some(signer) if signer == *owner => {}
        Some(signer) => bail!("SOC signed by {signer}, expected owner {owner}"),
        None => bail!("bad SOC signature"),
    }
    let expected = soc_address(&soc.identifier, owner);
    ensure!(
        expected == *address,
        "SOC address mismatch: chunk is {expected}, requested {address}"
    );
    Ok(soc.payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_verify_roundtrip() {
        let signer = PrivateKeySigner::random();
        let id = keccak256(b"eth:1337:hdr:0");
        let soc = sign(&signer, id, b"payload".to_vec()).unwrap();

        let mut raw = soc.identifier.to_vec();
        raw.extend_from_slice(&soc.signature);
        raw.extend_from_slice(&soc.span_and_payload());

        let addr = soc_address(&id, &signer.address());
        assert_eq!(verify(&raw, &addr, &signer.address()).unwrap(), b"payload");

        let other = PrivateKeySigner::random().address();
        assert!(verify(&raw, &soc_address(&id, &other), &other).is_err());

        let mut tampered = raw.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(verify(&tampered, &addr, &signer.address()).is_err());
    }
}
