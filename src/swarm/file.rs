//! The root reference of a Swarm file (`POST /bytes`), computed locally.
//!
//! Bee's chunker, unencrypted and without erasure coding (`Swarm-Redundancy-Level: 0`; Bee
//! 2.8.3 erasure-codes `/bytes` uploads unless told otherwise): the data is cut into
//! 4096-byte leaf chunks; up to 128 references (32 bytes each) form an intermediate chunk
//! whose span is the number of data bytes below it; levels are built until one reference is
//! left. When a level ends with a single reference that was not wrapped, that reference is
//! carried up unchanged instead of being wrapped on its own (Bee's hashtrie "carrier chunk").
//!
//! The address index needs this to put the reference of a spill or pages file into a bucket
//! before anything is uploaded, and independent importers need it to agree byte for byte.

use std::future::Future;

use alloy::primitives::B256;
use anyhow::{Result, ensure};

use super::bmt::{CHUNK_SIZE, cac_address};

/// References per intermediate chunk.
const BRANCHES: usize = CHUNK_SIZE / 32;

/// Root reference of `data` as Bee computes it for `POST /bytes`.
pub fn file_reference(data: &[u8]) -> B256 {
    let mut tree = ChunkTree::default();
    let mut none = |_: B256, _: u64, _: &[u8]| Ok(());
    for leaf in data.chunks(CHUNK_SIZE) {
        tree.push_leaf(leaf, &mut none)
            .expect("full leaves before the last");
    }
    tree.finish(&mut none).expect("no sink errors")
}

/// Where [`ChunkTree`] hands each chunk it makes: its address, span and payload.
pub type ChunkSink<'a> = dyn FnMut(B256, u64, &[u8]) -> Result<()> + 'a;

/// A file's chunk tree built leaf by leaf, handing out every chunk as it is made (leaves,
/// and each intermediate chunk once its references are complete): a file of any size is
/// chunked with memory for one path of the tree, at most 128 references per level. It is
/// the same tree as [`file_reference`]'s, which is built on it, so the chunks are the ones
/// Bee stores for `POST /bytes` of the whole file.
#[derive(Debug, Default)]
pub struct ChunkTree {
    /// References waiting on each level, with their spans.
    levels: Vec<Vec<(B256, u64)>>,
    leaves: u64,
    /// A leaf shorter than a chunk was pushed: it must be the last.
    short: bool,
}

impl ChunkTree {
    /// Adds the next leaf (1 to 4096 bytes; only the last may be shorter than 4096).
    pub fn push_leaf(&mut self, leaf: &[u8], out: &mut ChunkSink) -> Result<()> {
        ensure!(
            !leaf.is_empty() && leaf.len() <= CHUNK_SIZE,
            "a leaf of {} bytes",
            leaf.len()
        );
        ensure!(!self.short, "a leaf after a short one");
        self.short = leaf.len() < CHUNK_SIZE;
        let span = leaf.len() as u64;
        let address = cac_address(span, leaf);
        out(address, span, leaf)?;
        self.leaves += 1;
        self.push(0, (address, span), out)
    }

    /// Appends a reference to level `l`, wrapping the level as soon as it is full.
    fn push(&mut self, l: usize, r: (B256, u64), out: &mut ChunkSink) -> Result<()> {
        if self.levels.len() == l {
            self.levels.push(Vec::new());
        }
        self.levels[l].push(r);
        if self.levels[l].len() == BRANCHES {
            let wrapped = wrap(std::mem::take(&mut self.levels[l]), out)?;
            self.push(l + 1, wrapped, out)?;
        }
        Ok(())
    }

    /// The root reference, after handing out the chunks still open. Finishing from the
    /// bottom: an empty level is skipped, a single reference is carried up, anything else
    /// is wrapped into one chunk on the level above.
    pub fn finish(mut self, out: &mut ChunkSink) -> Result<B256> {
        if self.leaves == 0 {
            let address = cac_address(0, &[]);
            out(address, 0, &[])?;
            return Ok(address);
        }
        let mut l = 0;
        loop {
            let top = self.levels.iter().skip(l + 1).all(Vec::is_empty);
            match self.levels[l].len() {
                0 => {}
                1 if top => return Ok(self.levels[l][0].0),
                1 => {
                    let carried = self.levels[l].pop().unwrap();
                    self.levels[l + 1].push(carried);
                }
                _ => {
                    let wrapped = wrap(std::mem::take(&mut self.levels[l]), out)?;
                    if self.levels.len() == l + 1 {
                        self.levels.push(Vec::new());
                    }
                    self.levels[l + 1].push(wrapped);
                }
            }
            l += 1;
            if self.levels.len() == l {
                self.levels.push(Vec::new());
            }
        }
    }
}

/// A chunk in wire form: `span (u64 LE) ‖ payload`, as `POST /chunks` takes it.
pub fn chunk_wire(span: u64, payload: &[u8]) -> Vec<u8> {
    let mut w = Vec::with_capacity(8 + payload.len());
    w.extend_from_slice(&span.to_le_bytes());
    w.extend_from_slice(payload);
    w
}

/// Number of chunks Bee stores for a file of `len` bytes (leaves and intermediate chunks;
/// a carried single reference adds no chunk).
pub fn file_chunks(len: usize) -> usize {
    if len <= CHUNK_SIZE {
        return 1;
    }
    let mut refs = len.div_ceil(CHUNK_SIZE);
    let mut chunks = refs;
    while refs > 1 {
        let (full, rest) = (refs / BRANCHES, refs % BRANCHES);
        // Full groups are wrapped; a remainder of several references is wrapped too, a single
        // one is carried up as it is.
        chunks += full + usize::from(rest > 1);
        refs = full + usize::from(rest > 0);
    }
    chunks
}

/// One intermediate chunk over `refs`, handed out.
fn wrap(refs: Vec<(B256, u64)>, out: &mut ChunkSink) -> Result<(B256, u64)> {
    let span: u64 = refs.iter().map(|(_, s)| s).sum();
    let payload: Vec<u8> = refs.iter().flat_map(|(r, _)| r.0).collect();
    let address = cac_address(span, &payload);
    out(address, span, &payload)?;
    Ok((address, span))
}

/// One chunk of a file's tree as fetched (wire form `span ‖ payload`), checked against the
/// address it was fetched under, and its span against `expected` (what its parent says).
pub fn checked_chunk(wire: &[u8], address: &B256, expected: Option<u64>) -> Result<(u64, Vec<u8>)> {
    ensure!(
        wire.len() >= 8 && wire.len() <= 8 + CHUNK_SIZE,
        "chunk {address} of {} bytes",
        wire.len()
    );
    let span = u64::from_le_bytes(wire[..8].try_into().unwrap());
    let payload = &wire[8..];
    ensure!(
        cac_address(span, payload) == *address,
        "chunk does not hash to {address}"
    );
    if let Some(e) = expected {
        ensure!(
            span == e,
            "chunk {address} spans {span} bytes, its parent says {e}"
        );
    }
    if span <= CHUNK_SIZE as u64 {
        ensure!(
            payload.len() as u64 == span,
            "leaf {address} of {} bytes states span {span}",
            payload.len()
        );
    } else {
        let n = child_count(span);
        ensure!(
            payload.len() == 32 * n,
            "intermediate chunk {address} of span {span} has {} bytes of references, not {}",
            payload.len(),
            32 * n
        );
    }
    Ok((span, payload.to_vec()))
}

/// Bytes each child of an intermediate chunk of `span` bytes covers: the largest power of 128
/// leaves below the span (every child but the last covers exactly that).
fn child_cap(span: u64) -> u64 {
    let mut cap = CHUNK_SIZE as u64;
    while cap.saturating_mul(BRANCHES as u64) < span {
        cap *= BRANCHES as u64;
    }
    cap
}

fn child_count(span: u64) -> usize {
    span.div_ceil(child_cap(span)) as usize
}

/// The spans of the children of an intermediate chunk.
fn child_spans(span: u64) -> Vec<u64> {
    let (cap, n) = (child_cap(span), child_count(span) as u64);
    (0..n)
        .map(|i| if i + 1 < n { cap } else { span - cap * (n - 1) })
        .collect()
}

/// Fetches a chunk by address (wire form).
pub trait ChunkSource: Clone + Send + Sync + 'static {
    fn chunk(&self, address: B256) -> impl Future<Output = Result<Vec<u8>>> + Send;
}

impl ChunkSource for crate::swarm::bee::Bee {
    async fn chunk(&self, address: B256) -> Result<Vec<u8>> {
        self.get_chunk(&address).await
    }
}

/// A chunk source that keeps the intermediate chunks it has fetched: a walk to several leaves
/// of one file reads the path above them once.
#[derive(Clone)]
pub struct CachedChunks<S> {
    inner: S,
    cache: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<B256, Vec<u8>>>>,
}

impl<S> CachedChunks<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            cache: Default::default(),
        }
    }
}

impl<S: ChunkSource> ChunkSource for CachedChunks<S> {
    async fn chunk(&self, address: B256) -> Result<Vec<u8>> {
        if let Some(w) = self.cache.lock().unwrap().get(&address) {
            return Ok(w.clone());
        }
        let w = self.inner.chunk(address).await?;
        // Intermediate chunks (span above one chunk) only: leaves are read once.
        if w.len() >= 8 && u64::from_le_bytes(w[..8].try_into().unwrap()) > CHUNK_SIZE as u64 {
            self.cache.lock().unwrap().insert(address, w.clone());
        }
        Ok(w)
    }
}

/// The span of the file under `root`: its size.
pub async fn file_span<S: ChunkSource>(src: &S, root: B256) -> Result<u64> {
    Ok(checked_chunk(&src.chunk(root).await?, &root, None)?.0)
}

/// Reads the file under `root` leaf by leaf through its chunk tree, handing each leaf to
/// `leaf` in order, `concurrency` leaves fetched at a time; every chunk is checked against its
/// address, its span against its parent's, and the root's against `max_span`. Returns the
/// file's size. Memory is one tree path and the leaves in flight, whatever the file's size.
pub async fn read_leaves<S: ChunkSource>(
    src: &S,
    root: B256,
    max_span: u64,
    concurrency: usize,
    leaf: &mut (dyn FnMut(Vec<u8>) -> Result<()> + Send),
) -> Result<u64> {
    let (span, payload) = checked_chunk(&src.chunk(root).await?, &root, None)?;
    ensure!(
        span <= max_span,
        "file {root} of {span} bytes, above the {max_span} read"
    );
    walk(src, span, payload, concurrency.max(1), leaf).await?;
    Ok(span)
}

/// The leaves below a checked chunk, in order.
fn walk<'a, S: ChunkSource>(
    src: &'a S,
    span: u64,
    payload: Vec<u8>,
    concurrency: usize,
    leaf: &'a mut (dyn FnMut(Vec<u8>) -> Result<()> + Send),
) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        if span <= CHUNK_SIZE as u64 {
            return leaf(payload);
        }
        let children: Vec<(B256, u64)> = payload
            .chunks(32)
            .map(B256::from_slice)
            .zip(child_spans(span))
            .collect();
        if child_cap(span) == CHUNK_SIZE as u64 {
            // The children are leaves: fetched in parallel, handed out in order.
            for group in children.chunks(concurrency) {
                let mut set = tokio::task::JoinSet::new();
                for (i, (address, s)) in group.iter().copied().enumerate() {
                    let src = src.clone();
                    set.spawn(async move {
                        let wire = src.chunk(address).await?;
                        Ok::<_, anyhow::Error>((i, checked_chunk(&wire, &address, Some(s))?.1))
                    });
                }
                let mut got: Vec<Option<Vec<u8>>> = vec![None; group.len()];
                while let Some(done) = set.join_next().await {
                    let (i, p) =
                        done.map_err(|e| anyhow::anyhow!("a chunk read panicked: {e}"))??;
                    got[i] = Some(p);
                }
                for p in got {
                    leaf(p.expect("every leaf of the group was read"))?;
                }
            }
        } else {
            for (address, s) in children {
                let (s, p) = checked_chunk(&src.chunk(address).await?, &address, Some(s))?;
                walk(src, s, p, concurrency, leaf).await?;
            }
        }
        Ok(())
    })
}

/// Leaf `i` of the file under `root`, walking down from the root: one chunk per tree level,
/// each checked. An `i` past the file's last leaf is an error.
pub async fn read_leaf<S: ChunkSource>(src: &S, root: B256, i: u64) -> Result<Vec<u8>> {
    let (mut span, mut payload) = checked_chunk(&src.chunk(root).await?, &root, None)?;
    let leaves = span.div_ceil(CHUNK_SIZE as u64).max(1);
    ensure!(i < leaves, "leaf {i} of a file of {leaves} leaves");
    let mut offset = i * CHUNK_SIZE as u64;
    while span > CHUNK_SIZE as u64 {
        let cap = child_cap(span);
        let k = (offset / cap) as usize;
        let address = B256::from_slice(&payload[32 * k..32 * k + 32]);
        let s = child_spans(span)[k];
        (span, payload) = checked_chunk(&src.chunk(address).await?, &address, Some(s))?;
        offset -= k as u64 * cap;
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic test data.
    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    /// Chunks by address: span and payload.
    type Chunks = std::collections::HashMap<B256, (u64, Vec<u8>)>;

    /// Every chunk the tree hands out, by address, how many it handed out (identical chunks
    /// included), and the root.
    fn tree(d: &[u8]) -> (Chunks, usize, B256) {
        let mut chunks = std::collections::HashMap::new();
        let mut n = 0;
        let mut t = ChunkTree::default();
        let mut out = |a: B256, span: u64, p: &[u8]| {
            chunks.insert(a, (span, p.to_vec()));
            n += 1;
            Ok(())
        };
        for leaf in d.chunks(CHUNK_SIZE) {
            t.push_leaf(leaf, &mut out).unwrap();
        }
        let root = t.finish(&mut out).unwrap();
        (chunks, n, root)
    }

    /// The data below `r`, read through the chunks as a reader walks the tree.
    fn walk(chunks: &Chunks, r: &B256, out: &mut Vec<u8>) {
        let (span, payload) = &chunks[r];
        assert_eq!(cac_address(*span, payload), *r);
        if *span as usize <= CHUNK_SIZE {
            out.extend_from_slice(payload);
        } else {
            for c in payload.chunks(32) {
                walk(chunks, &B256::from_slice(c), out);
            }
        }
    }

    #[test]
    fn the_chunk_tree_hands_out_every_chunk_of_the_file() {
        for len in [
            0,
            1,
            4096,
            4097,
            128 * 4096,
            128 * 4096 + 1,
            129 * 4096 + 5,
            300 * 4096,
        ] {
            let d = data(len);
            let (chunks, n, root) = tree(&d);
            assert_eq!(root, file_reference(&d), "length {len}");
            assert_eq!(n, file_chunks(len), "length {len}");
            let mut back = Vec::new();
            walk(&chunks, &root, &mut back);
            assert_eq!(back, d, "length {len}");
        }
        // Only the last leaf may be short.
        let mut t = ChunkTree::default();
        let mut none = |_: B256, _: u64, _: &[u8]| Ok(());
        t.push_leaf(&[1; 10], &mut none).unwrap();
        assert!(t.push_leaf(&[1; 10], &mut none).is_err());
    }

    /// Chunks in memory, as a node serves them.
    #[derive(Clone, Default)]
    struct Store(std::sync::Arc<std::collections::HashMap<B256, Vec<u8>>>);

    impl ChunkSource for Store {
        async fn chunk(&self, address: B256) -> Result<Vec<u8>> {
            self.0
                .get(&address)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no chunk {address}"))
        }
    }

    fn store(d: &[u8]) -> (Store, B256) {
        let (chunks, _, root) = tree(d);
        let wire = chunks
            .into_iter()
            .map(|(a, (span, p))| (a, chunk_wire(span, &p)))
            .collect();
        (Store(std::sync::Arc::new(wire)), root)
    }

    #[tokio::test]
    async fn a_file_is_read_leaf_by_leaf_through_its_tree() {
        for len in [
            1usize,
            4096,
            4097,
            128 * 4096,
            128 * 4096 + 1,
            300 * 4096 + 7,
        ] {
            let d: Vec<u8> = (0..len)
                .map(|i| (i * 7 % 253) as u8 ^ (i / 4096) as u8)
                .collect();
            let (src, root) = store(&d);
            let mut back = Vec::new();
            let span = read_leaves(&src, root, u64::MAX, 8, &mut |l| {
                back.extend(l);
                Ok(())
            })
            .await
            .unwrap();
            assert_eq!((span, &back), (len as u64, &d), "length {len}");
            assert_eq!(file_span(&src, root).await.unwrap(), len as u64);
            let leaves = len.div_ceil(4096) as u64;
            for i in [0, leaves / 2, leaves - 1] {
                let start = i as usize * 4096;
                let want = &d[start..(start + 4096).min(len)];
                assert_eq!(
                    read_leaf(&src, root, i).await.unwrap(),
                    want,
                    "length {len}, leaf {i}"
                );
            }
            assert!(read_leaf(&src, root, leaves).await.is_err());
        }
        // A file above the limit, a chunk under the wrong address: refused.
        let (src, root) = store(&[1u8; 5000]);
        assert!(
            read_leaves(&src, root, 4999, 8, &mut |_| Ok(()))
                .await
                .is_err()
        );
        let mut bad = (*src.0).clone();
        let leaf = cac_address(4096, &[1u8; 4096]);
        bad.insert(leaf, chunk_wire(4096, &[2u8; 4096]));
        let bad = Store(std::sync::Arc::new(bad));
        assert!(
            read_leaves(&bad, root, u64::MAX, 8, &mut |_| Ok(()))
                .await
                .is_err()
        );
    }

    #[test]
    fn single_chunk_is_its_cac_address() {
        let d = data(4096);
        assert_eq!(file_reference(&d), cac_address(4096, &d));
    }

    #[test]
    fn chunk_counts() {
        for (len, chunks) in [
            (0, 1),
            (4096, 1),
            (4097, 3),
            // 128 leaves + their intermediate chunk.
            (128 * 4096, 129),
            // 129 leaves: one full intermediate chunk, the last leaf carried, and a root.
            (128 * 4096 + 1, 131),
            // 16384 leaves: 128 intermediates and a root.
            (16384 * 4096, 16513),
            // One more leaf, carried two levels up: 128 + 1 + 1 extra chunks.
            (16384 * 4096 + 1, 16515),
        ] {
            assert_eq!(file_chunks(len), chunks, "length {len}");
        }
    }

    #[test]
    fn two_leaves() {
        let d = data(4097);
        let a = cac_address(4096, &d[..4096]);
        let b = cac_address(1, &d[4096..]);
        let payload = [a.0, b.0].concat();
        assert_eq!(file_reference(&d), cac_address(4097, &payload));
    }

    /// References Bee 2.8.3 returned for `POST /bytes` with `Swarm-Redundancy-Level: 0` and
    /// [`data`] of these lengths (bee-factory, worker 1, 2026-11-18). Without that header
    /// this Bee erasure-codes the file, which changes the reference. They cover the
    /// level boundaries and the carried single reference.
    #[test]
    fn matches_bee_at_level_boundaries() {
        check(BEE_REFERENCES);
    }

    /// The two-levels-up cases hash 64 MiB, minutes in a debug build:
    /// `cargo test --release -- --ignored`.
    #[test]
    #[ignore]
    fn matches_bee_two_levels_up() {
        check(BEE_REFERENCES_LARGE);
    }

    /// The reader on the trees of the two-levels-up vectors (a reference carried two levels
    /// up, the shape of any spill file above 256 MB): 64 MiB, minutes in a debug build:
    /// `cargo test --release -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn files_two_levels_up_are_read_leaf_by_leaf() {
        for len in [67108865usize, 67112961] {
            let d = data(len);
            let (src, root) = store(&d);
            let mut back = Vec::with_capacity(len);
            read_leaves(&src, root, u64::MAX, 16, &mut |l| {
                back.extend(l);
                Ok(())
            })
            .await
            .unwrap();
            assert!(back == d, "length {len}");
            let leaves = len.div_ceil(4096) as u64;
            for i in [0, 16383, 16384, leaves - 1] {
                let start = i as usize * 4096;
                let want = &d[start..(start + 4096).min(len)];
                assert!(
                    read_leaf(&src, root, i).await.unwrap() == want,
                    "length {len}, leaf {i}"
                );
            }
        }
    }

    fn check(vectors: &[(usize, &str)]) {
        for (len, expected) in vectors {
            assert_eq!(
                file_reference(&data(*len)).to_string(),
                *expected,
                "length {len}"
            );
        }
    }

    const BEE_REFERENCES: &[(usize, &str)] = &[
        (
            1,
            "0xfe60ba40b87599ddfb9e8947c1c872a4a1a5b56f7d1b80f0a646005b38db52a5",
        ),
        (
            4096,
            "0x6400c281c45741b07de93a8d45c7f1505a6c2891b5eb89064c39718e62cd4d04",
        ),
        (
            4097,
            "0x09354b8aa9bf76e7a3f9591397b07c98ddf373e24d6aae7d69de01803df644be",
        ),
        (
            8192,
            "0x3073a2e8f0607eb980fdcd6593c0948e916cac1d76c6e1dd8028d73f79b85a10",
        ),
        // 128 leaves: one full intermediate chunk.
        (
            524287,
            "0xd064c5989933c26efb72e38884bbfd4b475ac369d7aafaeab2cf3098e6d5bbff",
        ),
        (
            524288,
            "0x7e28d401302a8ffb4091e8ae4b2ca78b5dc74125b416cfbed6370d11f692beca",
        ),
        // 129 leaves: the last leaf is carried up next to the full intermediate chunk.
        (
            524289,
            "0x63f72f51ebe9996af2d9c9ded7062484af525f1676603b838d91abd20b8a132d",
        ),
        (
            528384,
            "0xf37c8588cb133b31480a8f217dd3b813830ccd7b23d199cc156f5b666389f2dd",
        ),
        (
            528385,
            "0xdd2227d2dfe2d89be7918b97758068ecbd2d9a32a2e03bbefc4c98f9285b2bc1",
        ),
        (
            1052673,
            "0xa9d7c1cd9079e8e6c557e51bff6a33cb4bb830f7cd54d5d845fabf55f33fa179",
        ),
    ];

    const BEE_REFERENCES_LARGE: &[(usize, &str)] = &[
        // 128 × 128 leaves: a full second level; one more byte carries a leaf two levels up.
        (
            67108864,
            "0xdb1c8871f904d8ed0211f6b626db34ab9bd3b40a4bb32c8e290e0e86f95a3943",
        ),
        (
            67108865,
            "0x56798f8f37109429eaf993e018c134a5429bd698fda937fa84b2eddf75e2d151",
        ),
        (
            67112961,
            "0x9259a99399b1d22581ed420a5f0f92b3d7e7a5e1f8b3e982d04e8248e4c6efde",
        ),
    ];
}
