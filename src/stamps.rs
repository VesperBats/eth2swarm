//! `ethswarm stamps`: the Bee node's postage batches and what uses them, and `stamps plan`:
//! the depth, lifetime and cost of a batch for an export (`docs/address-index.md`,
//! *Lifetime*; for agents, `.claude/agent/postage.md`).
//! Nothing here buys, tops up or dilutes a batch: that is the owner's action, and `plan`
//! prints the command for it.

use std::path::{Path, PathBuf};

use alloy::primitives::B256;
use anyhow::{Context, Result, ensure};

use crate::addr::export;
use crate::swarm::bee::{BatchInfo, Bee};
use crate::swarm::postage::{
    BucketLoad, GNOSIS_BLOCK_SECS, MAX_DEPTH, amount_for, bucket_capacity, cost_plur, format_bzz,
};

/// `n` with thousands separators.
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fill(fullest: u64, cap: u64) -> String {
    format!(
        "{}/{} ({:.0} %)",
        group(fullest),
        group(cap),
        fullest as f64 * 100.0 / cap as f64
    )
}

fn ttl(secs: i64) -> String {
    if secs < 0 {
        "unknown".into()
    } else {
        format!("{:.1} days", secs as f64 / 86_400.0)
    }
}

/// An export and the batch its uploads used.
struct ExportUse {
    dir: PathBuf,
    owner: alloy::primitives::Address,
    chain_id: u64,
    batch: Option<B256>,
}

/// `ethswarm stamps`: one line per batch of the node, then which export uses which.
pub async fn list(bee: &Bee, exports: &[PathBuf]) -> Result<()> {
    let batches = bee.stamps().await.context("reading the node's batches")?;
    let uses: Vec<ExportUse> = exports
        .iter()
        .map(|dir| {
            let (meta, batch) = export::upload_batch(dir)?;
            Ok(ExportUse {
                dir: dir.clone(),
                owner: meta.owner,
                chain_id: meta.chain_id,
                batch,
            })
        })
        .collect::<Result<_>>()?;
    if batches.is_empty() {
        println!("The Bee node holds no postage batch.");
    }
    for b in &batches {
        print_batch(b);
        let mut named = false;
        for u in uses.iter().filter(|u| u.batch == Some(b.batch_id)) {
            named = true;
            println!(
                "    used by the export {} (owner {}, chain {})",
                u.dir.display(),
                u.owner,
                u.chain_id
            );
        }
        if !named {
            println!(
                "    use unknown: no given export names it (its label, or the work item's \
                 records, may)"
            );
        }
    }
    println!(
        "(Only an export's permanent batch is recorded, in its uploaded.json: a lower batch an \
         upload used shows as unknown here.)"
    );
    for u in &uses {
        match u.batch {
            None => println!(
                "The export {} (owner {}, chain {}) has not been uploaded yet.",
                u.dir.display(),
                u.owner,
                u.chain_id
            ),
            Some(id) if !batches.iter().any(|b| b.batch_id == id) => println!(
                "The export {} (owner {}, chain {}) was uploaded with batch {id}, which this \
                 node does not hold (another node's, or expired).",
                u.dir.display(),
                u.owner,
                u.chain_id
            ),
            Some(_) => {}
        }
    }
    Ok(())
}

fn print_batch(b: &BatchInfo) {
    let label = if b.label.is_empty() {
        String::new()
    } else {
        format!(" \"{}\"", b.label)
    };
    let mutability = match b.immutable {
        Some(true) => "immutable",
        Some(false) => "MUTABLE (the importer refuses it)",
        None => "mutability unknown (the importer refuses it)",
    };
    println!(
        "{}{label}: depth {}, fullest bucket {}, {mutability}, {}, lifetime {}",
        b.batch_id,
        b.depth,
        fill(b.utilization, b.bucket_capacity()),
        match (b.exists, b.usable) {
            (false, _) => "gone from the chain",
            (true, true) => "usable",
            (true, false) => "not usable yet",
        },
        ttl(b.ttl),
    );
}

/// What `stamps plan` is asked for.
pub struct PlanOptions {
    /// The lifetime to price, in days.
    pub days: Option<u64>,
    /// The highest share of a bucket the fullest one may take.
    pub max_fill: f64,
    /// Seconds per block of the chain the postage contract is on.
    pub block_secs: u64,
    pub label: Option<String>,
}

/// `ethswarm stamps plan <dir>`: the chunk load of the export's buckets, the depth that holds
/// it, and with `days` what that costs at the node's current price.
pub async fn plan(bee: &Bee, dir: &Path, opts: &PlanOptions) -> Result<()> {
    ensure!(
        opts.max_fill > 0.0 && opts.max_fill <= 1.0,
        "--max-fill must be in (0, 1], not {}",
        opts.max_fill
    );
    ensure!(opts.block_secs >= 1, "--block-secs must be at least 1");
    // Everything that can refuse the run is checked before the export is read: hashing a
    // large one takes a while.
    let priced = match opts.days {
        None => None,
        Some(days) => {
            ensure!(days >= 1, "--days must be at least 1");
            let secs = days
                .checked_mul(86_400)
                .with_context(|| format!("--days {days} is too long"))?;
            let state = bee
                .chain_state()
                .await
                .context("reading the storage price from the Bee node (GET /chainstate)")?;
            ensure!(
                state.current_price > 0,
                "the Bee node reports a storage price of 0"
            );
            let blocks = secs.div_ceil(opts.block_secs);
            if let Some(min) = state.minimum_validity_blocks {
                ensure!(
                    blocks > min,
                    "{days} days are {blocks} blocks of {} s; Bee refuses a balance of \
                     {min} blocks or fewer (its 24 h minimum): pass more --days",
                    opts.block_secs
                );
            }
            Some((days, secs, state))
        }
    };
    let mut all = BucketLoad::default();
    let mut permanent = BucketLoad::default();
    let mut lower = BucketLoad::default();
    let meta = export::for_each_chunk(dir, |address, life| {
        all.add(address);
        match life {
            None => permanent.add(address),
            Some(_) => lower.add(address),
        }
    })?;
    println!(
        "The export {} (owner {}, chain {}, layout {}) stamps {} chunks: fullest bucket {}, mean \
         {:.1}. A chunk written more than once (a merge a stopped run wrote again, leaves two \
         files share) is counted each time, so this is an upper bound: Bee may stamp a chunk \
         it stamped before with the same batch into the same slot.",
        dir.display(),
        meta.owner,
        meta.chain_id,
        meta.format,
        group(all.chunks()),
        group(all.fullest()),
        all.mean()
    );
    let mut sizes = vec![("one batch for everything", &all)];
    if lower.chunks() > 0 {
        println!(
            "  permanent records (heads, the origin, top-level slices): {} chunks, fullest \
             bucket {}",
            group(permanent.chunks()),
            group(permanent.fullest())
        );
        println!(
            "  lower slices (for --lower-batch): {} chunks, fullest bucket {}",
            group(lower.chunks()),
            group(lower.fullest())
        );
        sizes.push((
            "the permanent batch (--batch), lower slices all elsewhere",
            &permanent,
        ));
        sizes.push((
            "one lower batch taking every lower slice (--lower-batch)",
            &lower,
        ));
    }
    let sizes: Vec<(&str, u8, u64)> = sizes
        .into_iter()
        .map(|(what, load)| {
            let depth = load.depth_for(opts.max_fill).with_context(|| {
                format!("no batch up to depth {MAX_DEPTH} holds {what} at --max-fill")
            })?;
            Ok((what, depth, load.fullest()))
        })
        .collect::<Result<_>>()?;
    println!(
        "Depth, with the fullest bucket at most {:.0} % full (--max-fill):",
        opts.max_fill * 100.0
    );
    for (what, depth, fullest) in &sizes {
        println!(
            "  {what}: {depth} (fullest bucket {})",
            fill(*fullest, bucket_capacity(*depth))
        );
    }
    if lower.chunks() > 0 {
        println!(
            "A lower slice that no --lower-batch outlives, or whose batch is not usable yet, \
             goes to the permanent batch. Size --batch like the first line unless the lower \
             batches surely cover every lower slice; otherwise an immutable --batch refuses \
             chunks once its fullest bucket is full."
        );
    }
    let Some((days, secs, state)) = priced else {
        println!("Pass --days N for the cost of keeping it N days at the current price.");
        return Ok(());
    };
    let amount = amount_for(state.current_price, secs, opts.block_secs);
    if let Some(min) = state.minimum_validity_blocks
        && secs.div_ceil(opts.block_secs) <= min + min / 4
    {
        println!(
            "Close to Bee's 24 h minimum ({min} blocks): a price rise before the purchase makes \
             Bee refuse it. Add a day to be safe."
        );
    }
    println!(
        "{days} days at the current price ({} PLUR per chunk per block, block {}, {} s per \
         block): a balance of {amount} PLUR per chunk. The price moves; this is an estimate.",
        state.current_price, state.block, opts.block_secs
    );
    for (what, depth, _) in &sizes {
        println!(
            "  {what}, depth {depth}: {}",
            format_bzz(cost_plur(amount, *depth))
        );
    }
    println!(
        "To buy one (the owner's action; nothing here buys, and the importer refuses a \
         mutable batch):"
    );
    for (what, depth, _) in &sizes {
        println!(
            "  {what}: {}",
            buy_command(bee.base_url(), amount, *depth, opts.label.as_deref())?
        );
    }
    if opts.label.is_none() {
        println!("  (--label names the batch: owner, chain and first block of the index.)");
    }
    if lower.chunks() > 0 {
        println!(
            "Every line is priced for --days and named by --label. A lower batch needs only \
             its slices' lifetime plus --grace-days, and a label of its own: run plan again \
             with those for the lower line."
        );
    }
    Ok(())
}

/// `s` as one shell word in single quotes (a `'` inside closes, escapes and reopens).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The command that buys an immutable batch of `depth` with `amount` per chunk on the node at
/// `base`: the label percent-encoded in the URL, the URL shell-quoted.
fn buy_command(
    base: &alloy::transports::http::reqwest::Url,
    amount: u128,
    depth: u8,
    label: Option<&str>,
) -> Result<String> {
    let mut url = base
        .join(&format!("stamps/{amount}/{depth}"))
        .context("building the purchase URL")?;
    if let Some(label) = label {
        url.query_pairs_mut().append_pair("label", label);
    }
    Ok(format!(
        "curl -X POST -H 'immutable: true' {}",
        shell_quote(url.as_str())
    ))
}

/// The default seconds per block for `--block-secs`.
pub const DEFAULT_BLOCK_SECS: u64 = GNOSIS_BLOCK_SECS;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_read_easily() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1_000), "1,000");
        assert_eq!(group(689_736), "689,736");
        assert_eq!(group(1_234_567_890), "1,234,567,890");
        assert_eq!(fill(32, bucket_capacity(23)), "32/128 (25 %)");
        assert_eq!(ttl(-1), "unknown");
        assert_eq!(ttl(86_400 * 3 / 2), "1.5 days");
    }

    #[test]
    fn the_buy_command_survives_any_label() {
        let base: alloy::transports::http::reqwest::Url = "http://localhost:1635/".parse().unwrap();
        assert_eq!(
            buy_command(&base, 1200, 22, None).unwrap(),
            "curl -X POST -H 'immutable: true' 'http://localhost:1635/stamps/1200/22'"
        );
        assert_eq!(
            buy_command(&base, 1200, 22, Some("0xabc, chain 1 & owner's <x>")).unwrap(),
            "curl -X POST -H 'immutable: true' \
             'http://localhost:1635/stamps/1200/22?label=0xabc%2C+chain+1+%26+owner%27s+%3Cx%3E'"
        );
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
