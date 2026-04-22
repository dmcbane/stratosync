//! Content-equality helpers for local and remote files.
//!
//! An ETag mismatch between the DB and the remote doesn't guarantee that the
//! bytes differ — cloud providers can change a file's ETag for reasons
//! unrelated to content (re-upload of identical bytes, metadata-only
//! modifications, provider-side hash format changes). These helpers perform
//! an authoritative byte-for-byte comparison so callers can distinguish real
//! conflicts from spurious ones.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tokio::io::AsyncReadExt;

use crate::backend::Backend;
use crate::types::RemoteMetadata;

/// Compare a local file with a remote file byte-for-byte.
///
/// Returns `(equal, remote_meta)`. The returned metadata is always the fresh
/// stat of the remote, so callers can refresh their DB ETag regardless of
/// whether the bytes matched.
///
/// Short-circuits on size mismatch without downloading.
pub async fn local_eq_remote(
    local_path: &Path,
    remote_path: &str,
    backend: &Arc<dyn Backend>,
) -> Result<(bool, RemoteMetadata)> {
    let remote_meta = backend.stat(remote_path).await
        .map_err(|e| anyhow::anyhow!("stat {remote_path}: {e}"))?;

    let local_size = tokio::fs::metadata(local_path).await?.len();
    if local_size != remote_meta.size {
        return Ok((false, remote_meta));
    }

    let tmp_path = unique_tmp_sibling(local_path, "eq-check");
    if let Err(e) = backend.download(remote_path, &tmp_path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(anyhow::anyhow!("download {remote_path}: {e}"));
    }

    let equal = files_equal(local_path, &tmp_path, remote_meta.size).await;
    let _ = tokio::fs::remove_file(&tmp_path).await;
    Ok((equal?, remote_meta))
}

/// Three-valued result of a stat-only (no-download) remote-to-remote
/// equality check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatEqResult {
    /// Sizes match and content hashes match — bytes are equal.
    Equal,
    /// Sizes differ, or content hashes exist on both sides but differ.
    Different,
    /// Can't decide from stat alone (hash absent on one/both sides).
    /// Caller should fall back to `remote_eq_remote`.
    Unknown,
}

/// Quick remote-to-remote equality check that uses only `stat` (no download).
///
/// - Size mismatch → `Different` (cheap).
/// - Both sizes match AND both ETags are present AND equal → `Equal`.
/// - Both sizes match AND both ETags are present AND differ → `Different`.
/// - Any ETag missing → `Unknown`.
///
/// For content-hash-based ETags (Google Drive MD5, OneDrive SHA1, etc.) this
/// is authoritative. For providers whose ETag is a file ID rather than a
/// content hash, equal ETags still mean "same object" (which for cleanup
/// purposes is still fine — we wouldn't be removing a sibling we shouldn't).
pub async fn stat_remote_eq(
    path_a: &str,
    path_b: &str,
    backend: &Arc<dyn Backend>,
) -> Result<StatEqResult> {
    let meta_a = backend.stat(path_a).await
        .map_err(|e| anyhow::anyhow!("stat {path_a}: {e}"))?;
    let meta_b = backend.stat(path_b).await
        .map_err(|e| anyhow::anyhow!("stat {path_b}: {e}"))?;
    if meta_a.size != meta_b.size {
        return Ok(StatEqResult::Different);
    }
    match (&meta_a.etag, &meta_b.etag) {
        (Some(a), Some(b)) if a == b => Ok(StatEqResult::Equal),
        (Some(_), Some(_))           => Ok(StatEqResult::Different),
        _                             => Ok(StatEqResult::Unknown),
    }
}

/// Compare two remote files byte-for-byte by downloading both to a work directory.
/// Short-circuits on size mismatch without downloading.
pub async fn remote_eq_remote(
    path_a: &str,
    path_b: &str,
    backend: &Arc<dyn Backend>,
    work_dir: &Path,
) -> Result<bool> {
    let meta_a = backend.stat(path_a).await
        .map_err(|e| anyhow::anyhow!("stat {path_a}: {e}"))?;
    let meta_b = backend.stat(path_b).await
        .map_err(|e| anyhow::anyhow!("stat {path_b}: {e}"))?;
    if meta_a.size != meta_b.size {
        return Ok(false);
    }

    tokio::fs::create_dir_all(work_dir).await.ok();
    let tmp_a = unique_tmp_in(work_dir, "eq-a");
    let tmp_b = unique_tmp_in(work_dir, "eq-b");

    let dl_a = backend.download(path_a, &tmp_a).await;
    let dl_b = backend.download(path_b, &tmp_b).await;

    let result = match (dl_a, dl_b) {
        (Ok(()), Ok(())) => files_equal(&tmp_a, &tmp_b, meta_a.size).await,
        (Err(e), _) => Err(anyhow::anyhow!("download {path_a}: {e}")),
        (_, Err(e)) => Err(anyhow::anyhow!("download {path_b}: {e}")),
    };

    let _ = tokio::fs::remove_file(&tmp_a).await;
    let _ = tokio::fs::remove_file(&tmp_b).await;
    result
}

async fn files_equal(a: &Path, b: &Path, size: u64) -> Result<bool> {
    let mut fa = tokio::fs::File::open(a).await?;
    let mut fb = tokio::fs::File::open(b).await?;
    let mut buf_a = vec![0u8; 65536];
    let mut buf_b = vec![0u8; 65536];
    let mut remaining = size;
    while remaining > 0 {
        let chunk = remaining.min(buf_a.len() as u64) as usize;
        fa.read_exact(&mut buf_a[..chunk]).await?;
        fb.read_exact(&mut buf_b[..chunk]).await?;
        if buf_a[..chunk] != buf_b[..chunk] {
            return Ok(false);
        }
        remaining -= chunk as u64;
    }
    Ok(true)
}

fn unique_tmp_sibling(base: &Path, tag: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos()).unwrap_or(0);
    base.with_extension(format!("stratosync-{tag}-{pid}-{nanos}"))
}

fn unique_tmp_in(dir: &Path, tag: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos()).unwrap_or(0);
    dir.join(format!("stratosync-{tag}-{pid}-{nanos}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;

    #[tokio::test]
    async fn local_eq_remote_returns_true_for_identical_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.bin");
        std::fs::write(&local, b"hello world").unwrap();

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::default());
        backend.upload(&local, "a.bin", None).await.unwrap();

        let (equal, _meta) = local_eq_remote(&local, "a.bin", &backend).await.unwrap();
        assert!(equal);
    }

    #[tokio::test]
    async fn local_eq_remote_returns_false_for_different_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.bin");
        std::fs::write(&local, b"local version").unwrap();

        let remote_seed = dir.path().join("seed.bin");
        std::fs::write(&remote_seed, b"remote version").unwrap();
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::default());
        backend.upload(&remote_seed, "a.bin", None).await.unwrap();

        let (equal, _meta) = local_eq_remote(&local, "a.bin", &backend).await.unwrap();
        assert!(!equal);
    }

    #[tokio::test]
    async fn local_eq_remote_size_mismatch_skips_download() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.bin");
        std::fs::write(&local, b"short").unwrap();

        let remote_seed = dir.path().join("seed.bin");
        std::fs::write(&remote_seed, b"much longer content here").unwrap();
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::default());
        backend.upload(&remote_seed, "a.bin", None).await.unwrap();

        let (equal, meta) = local_eq_remote(&local, "a.bin", &backend).await.unwrap();
        assert!(!equal);
        assert_eq!(meta.size, 24);
    }

    #[tokio::test]
    async fn stat_remote_eq_size_mismatch_is_different() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.seed");
        let b = dir.path().join("b.seed");
        std::fs::write(&a, b"short").unwrap();
        std::fs::write(&b, b"longer content here").unwrap();

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::default());
        backend.upload(&a, "a.bin", None).await.unwrap();
        backend.upload(&b, "b.bin", None).await.unwrap();

        let result = stat_remote_eq("a.bin", "b.bin", &backend).await.unwrap();
        assert_eq!(result, StatEqResult::Different);
    }

    #[tokio::test]
    async fn remote_eq_remote_identical() {
        let dir = tempfile::tempdir().unwrap();
        let seed = dir.path().join("seed.bin");
        std::fs::write(&seed, b"same bytes on both paths").unwrap();

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::default());
        backend.upload(&seed, "a.bin", None).await.unwrap();
        backend.upload(&seed, "b.bin", None).await.unwrap();

        let equal = remote_eq_remote("a.bin", "b.bin", &backend, dir.path()).await.unwrap();
        assert!(equal);
    }

    #[tokio::test]
    async fn remote_eq_remote_different() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.seed");
        let b = dir.path().join("b.seed");
        std::fs::write(&a, b"content a").unwrap();
        std::fs::write(&b, b"content b").unwrap();

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::default());
        backend.upload(&a, "a.bin", None).await.unwrap();
        backend.upload(&b, "b.bin", None).await.unwrap();

        let equal = remote_eq_remote("a.bin", "b.bin", &backend, dir.path()).await.unwrap();
        assert!(!equal);
    }
}
