//! Small helpers the importer and the export share.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Writes a small file durably: a temporary file, synced, renamed, and the directory synced,
/// so that after a crash or a power loss it is either the old or the new content.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(data)
        .and_then(|_| f.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    if let Some(dir) = path.parent() {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("syncing {}", dir.display()))?;
    }
    Ok(())
}

/// Waits for one upload task, if any is running.
pub async fn join_one(set: &mut tokio::task::JoinSet<Result<()>>) -> Result<()> {
    match set.join_next().await {
        Some(r) => r.context("upload task panicked")?,
        None => Ok(()),
    }
}
