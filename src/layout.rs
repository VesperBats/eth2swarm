//! On-Swarm layout: SOC identifiers and the binary payloads stored in them.
//!
//! | SOC identifier                          | payload            |
//! |-----------------------------------------|--------------------|
//! | `keccak256("eth:<chain>:<anchor>:hdr:<n>")`   | [`HeaderRecord`]   |
//! | `keccak256("eth:<chain>:<anchor>:epoch:<k>")` | [`EpochRecord`]    |
//! | `keccak256("eth:<chain>:accumulator")`        | [`AnchorRecord`]   |
//!
//! Header and epoch identifiers are namespaced by the trust anchor (lower-case hex with
//! `0x`), so every accumulator lives in its own immutable set of SOCs: re-importing with a
//! different range never overwrites them. Only the anchor pointer is mutable.
//!
//! Block bodies and per-epoch header files are ordinary Swarm files (`/bytes`), referenced
//! from the records. All integers are big-endian. Every record starts with
//! `MAGIC ‖ VERSION ‖ kind`.

use alloy::primitives::{B256, keccak256};
use anyhow::{Result, bail, ensure};

const MAGIC: &[u8; 3] = b"E2S";
const VERSION: u8 = 1;
const KIND_HEADER: u8 = 1;
const KIND_EPOCH: u8 = 2;
const KIND_ANCHOR: u8 = 3;

pub fn header_id(chain_id: u64, anchor: &B256, number: u64) -> B256 {
    keccak256(format!("eth:{chain_id}:{anchor}:hdr:{number}"))
}

pub fn epoch_id(chain_id: u64, anchor: &B256, epoch: u64) -> B256 {
    keccak256(format!("eth:{chain_id}:{anchor}:epoch:{epoch}"))
}

pub fn anchor_id(chain_id: u64) -> B256 {
    keccak256(format!("eth:{chain_id}:accumulator"))
}

/// Accumulator geometry, carried in every record so a client can recompute the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub epoch_size: u32,
    pub num_epochs: u32,
}

/// Per-block record: the header plus everything needed to verify it against the anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderRecord {
    pub geometry: Geometry,
    /// Canonical RLP encoding of the header (its keccak256 is the block hash).
    pub header_rlp: Vec<u8>,
    /// Swarm reference of the RLP-encoded block body (transactions, ommers, withdrawals).
    pub body_ref: B256,
    /// Siblings from the block hash up to its epoch root.
    pub epoch_proof: Vec<B256>,
    /// Siblings from the epoch root up to the master tree root.
    pub master_proof: Vec<B256>,
}

/// Per-epoch record: epoch root, the file of all headers in the epoch, and its master proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochRecord {
    pub geometry: Geometry,
    pub epoch_root: B256,
    /// Swarm reference of the concatenated header RLPs of the epoch.
    pub headers_ref: B256,
    pub master_proof: Vec<B256>,
}

/// Published trust anchor (convenience: a client should pin the anchor out of band).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorRecord {
    pub geometry: Geometry,
    pub anchor: B256,
}

/// Record encoder: `MAGIC ‖ VERSION ‖ kind`, then big-endian fields. Shared with the
/// address index records (`addr::layout`).
pub(crate) struct Writer(pub(crate) Vec<u8>);

impl Writer {
    pub(crate) fn new(kind: u8) -> Self {
        let mut buf = MAGIC.to_vec();
        buf.extend([VERSION, kind]);
        Self(buf)
    }
    fn geometry(&mut self, g: Geometry) {
        self.0.extend(g.epoch_size.to_be_bytes());
        self.0.extend(g.num_epochs.to_be_bytes());
    }
    pub(crate) fn hash(&mut self, h: &B256) {
        self.0.extend_from_slice(h.as_slice());
    }
    pub(crate) fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.0.extend(v.to_be_bytes());
    }
    pub(crate) fn u64(&mut self, v: u64) {
        self.0.extend(v.to_be_bytes());
    }
    fn hashes(&mut self, hs: &[B256]) {
        self.0
            .push(u8::try_from(hs.len()).expect("proof longer than 255"));
        hs.iter().for_each(|h| self.hash(h));
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend(
            u16::try_from(b.len())
                .expect("field over 64 KiB")
                .to_be_bytes(),
        );
        self.0.extend_from_slice(b);
    }
}

/// Record decoder matching [`Writer`].
pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8], kind: u8) -> Result<Self> {
        ensure!(
            buf.len() >= 5 && &buf[..3] == MAGIC,
            "not an eth2swarm record"
        );
        ensure!(buf[3] == VERSION, "unsupported record version {}", buf[3]);
        ensure!(buf[4] == kind, "record kind {} (expected {kind})", buf[4]);
        Ok(Self(&buf[5..]))
    }
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.0.len() >= n, "record truncated");
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }
    fn geometry(&mut self) -> Result<Geometry> {
        Ok(Geometry {
            epoch_size: self.u32()?,
            num_epochs: self.u32()?,
        })
    }
    pub(crate) fn hash(&mut self) -> Result<B256> {
        Ok(B256::from_slice(self.take(32)?))
    }
    fn hashes(&mut self) -> Result<Vec<B256>> {
        let n = self.take(1)?[0];
        (0..n).map(|_| self.hash()).collect()
    }
    fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u16()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    pub(crate) fn finish(self) -> Result<()> {
        if !self.0.is_empty() {
            bail!("{} trailing bytes in record", self.0.len());
        }
        Ok(())
    }
}

impl HeaderRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new(KIND_HEADER);
        w.geometry(self.geometry);
        w.bytes(&self.header_rlp);
        w.hash(&self.body_ref);
        w.hashes(&self.epoch_proof);
        w.hashes(&self.master_proof);
        w.0
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf, KIND_HEADER)?;
        let rec = Self {
            geometry: r.geometry()?,
            header_rlp: r.bytes()?,
            body_ref: r.hash()?,
            epoch_proof: r.hashes()?,
            master_proof: r.hashes()?,
        };
        r.finish()?;
        Ok(rec)
    }
}

impl EpochRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new(KIND_EPOCH);
        w.geometry(self.geometry);
        w.hash(&self.epoch_root);
        w.hash(&self.headers_ref);
        w.hashes(&self.master_proof);
        w.0
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf, KIND_EPOCH)?;
        let rec = Self {
            geometry: r.geometry()?,
            epoch_root: r.hash()?,
            headers_ref: r.hash()?,
            master_proof: r.hashes()?,
        };
        r.finish()?;
        Ok(rec)
    }
}

impl AnchorRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new(KIND_ANCHOR);
        w.geometry(self.geometry);
        w.hash(&self.anchor);
        w.0
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf, KIND_ANCHOR)?;
        let rec = Self {
            geometry: r.geometry()?,
            anchor: r.hash()?,
        };
        r.finish()?;
        Ok(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_roundtrip() {
        let g = Geometry {
            epoch_size: 64,
            num_epochs: 4,
        };
        let h = HeaderRecord {
            geometry: g,
            header_rlp: vec![0xf9, 1, 2, 3],
            body_ref: keccak256(b"body"),
            epoch_proof: vec![keccak256(b"a"); 6],
            master_proof: vec![keccak256(b"b"); 2],
        };
        assert_eq!(HeaderRecord::decode(&h.encode()).unwrap(), h);

        let e = EpochRecord {
            geometry: g,
            epoch_root: keccak256(b"r"),
            headers_ref: keccak256(b"f"),
            master_proof: vec![keccak256(b"c"); 2],
        };
        assert_eq!(EpochRecord::decode(&e.encode()).unwrap(), e);
        assert!(HeaderRecord::decode(&e.encode()).is_err());

        let a = AnchorRecord {
            geometry: g,
            anchor: keccak256(b"anchor"),
        };
        assert_eq!(AnchorRecord::decode(&a.encode()).unwrap(), a);
    }
}
