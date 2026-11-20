//! Importer: chain → Swarm.

use crate::accumulator::{self, Tree};
use crate::chain::{CanonicalBlock, fetch_block};
use crate::layout::{self, AnchorRecord, EpochRecord, Geometry, HeaderRecord};
use crate::swarm::{bee::Bee, soc};
use alloy::primitives::B256;
use alloy::providers::Provider;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, ensure};
use tracing::info;

pub struct ImportOptions {
    pub epoch_size: u32,
    pub num_epochs: u32,
    pub batch: Option<B256>,
}

/// Import blocks `0 .. num_epochs * epoch_size` and publish the accumulator.
/// Returns the trust anchor.
pub async fn run(
    provider: &impl Provider,
    bee: &Bee,
    signer: &PrivateKeySigner,
    opts: ImportOptions,
) -> Result<B256> {
    ensure!(
        opts.epoch_size.is_power_of_two(),
        "--epoch-size must be a power of two"
    );
    ensure!(opts.num_epochs > 0, "--epochs must be at least 1");
    let geometry = Geometry {
        epoch_size: opts.epoch_size,
        num_epochs: opts.num_epochs,
    };
    let chain_id = provider.get_chain_id().await.context("querying chain id")?;
    let tip = provider.get_block_number().await.context("querying tip")?;
    let total = u64::from(opts.epoch_size) * u64::from(opts.num_epochs);
    ensure!(
        total <= tip + 1,
        "{total} blocks requested but the chain has only {} (tip #{tip})",
        tip + 1
    );
    let batch = match opts.batch {
        Some(b) => b,
        None => bee.usable_batch().await?,
    };
    bee.immutable_batch(&batch, "the batch (--batch)").await?;
    let owner = signer.address();
    info!(%chain_id, %owner, %batch, total, "importing");

    // 1. Read the chain and build the accumulator.
    let mut blocks: Vec<CanonicalBlock> = Vec::with_capacity(total as usize);
    for n in 0..total {
        let block = fetch_block(provider, n).await?;
        if let Some(prev) = blocks.last() {
            ensure!(
                block.header.parent_hash == prev.hash,
                "block {n} does not link to block {}",
                n - 1
            );
        }
        blocks.push(block);
        if (n + 1) % 256 == 0 {
            info!("fetched {}/{total} blocks", n + 1);
        }
    }
    let epoch_depth = accumulator::depth_for(opts.epoch_size as usize);
    let epoch_trees: Vec<Tree> = blocks
        .chunks(opts.epoch_size as usize)
        .map(|epoch| {
            let hashes: Vec<B256> = epoch.iter().map(|b| b.hash).collect();
            Tree::new(&hashes, epoch_depth)
        })
        .collect();
    let epoch_roots: Vec<B256> = epoch_trees.iter().map(Tree::root).collect();
    let master = Tree::new(
        &epoch_roots,
        accumulator::depth_for(opts.num_epochs as usize),
    );
    let anchor = accumulator::anchor(
        master.root(),
        opts.num_epochs.into(),
        opts.epoch_size.into(),
    );

    // 2. Publish epochs, bodies and per-block header SOCs.
    let mut max_header_soc = 0;
    for (k, (epoch, tree)) in blocks
        .chunks(opts.epoch_size as usize)
        .zip(&epoch_trees)
        .enumerate()
    {
        let headers_file: Vec<u8> = epoch.iter().flat_map(|b| b.header_rlp.clone()).collect();
        let headers_ref = bee.upload_bytes(&batch, headers_file).await?;
        let master_proof = master.proof(k);
        let rec = EpochRecord {
            geometry,
            epoch_root: tree.root(),
            headers_ref,
            master_proof: master_proof.clone(),
        };
        publish(
            bee,
            &batch,
            signer,
            layout::epoch_id(chain_id, &anchor, k as u64),
            rec.encode(),
        )
        .await?;

        for (i, block) in epoch.iter().enumerate() {
            let body_ref = bee.upload_bytes(&batch, block.body_rlp.clone()).await?;
            let rec = HeaderRecord {
                geometry,
                header_rlp: block.header_rlp.clone(),
                body_ref,
                epoch_proof: tree.proof(i),
                master_proof: master_proof.clone(),
            };
            let payload = rec.encode();
            max_header_soc = max_header_soc.max(payload.len());
            publish(
                bee,
                &batch,
                signer,
                layout::header_id(chain_id, &anchor, block.number),
                payload,
            )
            .await?;
        }
        info!(epoch = k, root = %tree.root(), %headers_ref, "epoch published");
    }

    let rec = AnchorRecord { geometry, anchor };
    publish(
        bee,
        &batch,
        signer,
        layout::anchor_id(chain_id),
        rec.encode(),
    )
    .await?;

    println!("Chain ID:        {chain_id}");
    println!("Owner:           {owner}");
    println!("Blocks:          0..={}", total - 1);
    println!("Epochs:          {} × {}", opts.num_epochs, opts.epoch_size);
    println!("Max header SOC:  {max_header_soc} bytes payload");
    println!("Trust anchor:    {anchor}");
    Ok(anchor)
}

async fn publish(
    bee: &Bee,
    batch: &B256,
    signer: &PrivateKeySigner,
    id: B256,
    payload: Vec<u8>,
) -> Result<()> {
    let owner = signer.address();
    let soc = soc::sign(signer, id, payload)?;
    let addr = bee.upload_soc(batch, &owner, &soc).await?;
    let expected = soc::soc_address(&id, &owner);
    ensure!(
        addr == expected,
        "Bee stored SOC at {addr}, expected {expected}"
    );
    Ok(())
}
