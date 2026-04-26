//! File versioning helper.
//!
//! Captures a snapshot of file content into the existing content-addressed
//! `BaseStore` and records a row in `version_history`. After insert, prunes
//! older history beyond `retention` and removes blob files that have lost
//! their last reference (across both `version_history` and `base_versions`).
//!
//! Skips files larger than `max_size` to keep the on-disk version cache
//! bounded — large media files would dominate disk usage and aren't the
//! sort of content users typically need to roll back.
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, warn};

use stratosync_core::{
    base_store::BaseStore,
    state::{StateDb, VersionSource},
    types::Inode,
};

/// Capture a single snapshot of `cache_path` into the version store.
///
/// No-op (returns Ok) when `retention == 0` — the user has versioning
/// turned off — or when `size > max_size`.
pub async fn capture(
    db:        &Arc<StateDb>,
    bs:        &Arc<BaseStore>,
    inode:     Inode,
    mount_id:  u32,
    cache_path: &Path,
    size:      u64,
    etag:      Option<&str>,
    source:    VersionSource,
    max_size:  u64,
    retention: u32,
) -> Result<()> {
    if retention == 0 || size > max_size {
        return Ok(());
    }
    if !cache_path.exists() {
        // Nothing to snapshot — file was never hydrated. This is a normal
        // case for placeholder-only entries; not an error.
        debug!(inode, ?cache_path, "skipping version snapshot — no cache file");
        return Ok(());
    }

    // store_base is content-addressed and dedup'd, so running it on
    // identical content (e.g. a poll that arrives twice) is cheap.
    let cp = cache_path.to_owned();
    let bs_c = Arc::clone(bs);
    let hash = tokio::task::spawn_blocking(move || bs_c.store_base(&cp))
        .await
        .map_err(|e| anyhow::anyhow!("store_base panicked: {e}"))??;

    db.insert_version_history(inode, mount_id, &hash, size, etag, source).await?;

    // Trim history; delete now-unreferenced blobs from disk.
    let orphans = db.prune_version_history(inode, retention).await?;
    for h in orphans {
        if let Err(e) = bs.remove_object(&h) {
            warn!(hash = %h, "failed to remove orphaned base blob: {e}");
        }
    }

    debug!(inode, source = source.as_str(), %hash, "captured version");
    Ok(())
}
