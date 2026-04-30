//! Per-inode "header" cache for the first N bytes of large files.
//!
//! File managers (Dolphin / KIO, Nautilus + thumbnailers) routinely
//! `open()` + `read(offset=0, len=16-64 KiB)` on every selected file —
//! MIME-type sniffing, EXIF / video header reads for thumbnails, etc.
//! When the underlying file is `Remote`, each of those reads becomes a
//! `rclone cat --offset 0 --count …` invocation, which is process-spawn
//! + OAuth refresh + cloud round-trip ≈ 1–3 s. Right-click on N selected
//! large files therefore blocks the UI for ~3 s × N — the user observed
//! 30 s context-menu hangs on a folder of MP4s.
//!
//! This module persists a small header (default 64 KiB) per file under
//! `<cache_dir>/.meta/headers/<inode>` and exposes `read_header` so the
//! FUSE `read` handler can short-circuit those tiny offset-0 sniff
//! reads. Header files are owner-only (0o600) and atomically replaced
//! via temp-rename. Misses fall through to the existing range-download
//! path, so this is purely a latency optimization — never a correctness
//! boundary.
use std::path::{Path, PathBuf};

use stratosync_core::types::Inode;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub fn header_path(cache_dir: &Path, inode: Inode) -> PathBuf {
    cache_dir.join(".meta").join("headers").join(inode.to_string())
}

/// Returns `Ok(Some(bytes))` if a header file exists for `inode` and
/// fully covers the requested `[offset, offset+len)` range. Returns
/// `Ok(None)` when no header is on disk OR the range extends past what
/// we cached — the caller should fall through to the network in both
/// cases. `Err(_)` is reserved for genuine I/O failures (permissions,
/// disk full, etc.).
pub async fn read_header(
    cache_dir: &Path,
    inode:     Inode,
    offset:    u64,
    len:       u64,
) -> std::io::Result<Option<Vec<u8>>> {
    if len == 0 {
        return Ok(Some(Vec::new()));
    }
    let path = header_path(cache_dir, inode);
    let mut f = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = f.metadata().await?;
    if offset.saturating_add(len) > meta.len() {
        return Ok(None); // request runs past the cached header
    }
    f.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; len as usize];
    let n = f.read(&mut buf).await?;
    buf.truncate(n);
    Ok(Some(buf))
}

/// Write `data` as the header file for `inode`. Uses a temp-rename so
/// concurrent readers either see the old contents or the new — never a
/// half-written file. Sets owner-only permissions on Unix.
pub async fn write_header(
    cache_dir: &Path,
    inode:     Inode,
    data:      &[u8],
) -> std::io::Result<()> {
    let path = header_path(cache_dir, inode);
    let dir = path.parent()
        .ok_or_else(|| std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "header path has no parent dir",
        ))?;
    tokio::fs::create_dir_all(dir).await?;
    // Make the parent owner-only (in case it didn't exist) — same
    // posture as the existing `.meta/partial` dir.
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await.ok();

    let tmp = path.with_extension(format!(
        "tmp.{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos()).unwrap_or(0),
    ));
    tokio::fs::write(&tmp, data).await?;
    tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

/// Drop the header for `inode` if one exists. ENOENT is not an error.
pub async fn invalidate_header(cache_dir: &Path, inode: Inode) {
    let path = header_path(cache_dir, inode);
    if let Err(e) = tokio::fs::remove_file(&path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::debug!(inode, ?path, "header invalidate failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let payload = vec![0xABu8; 1024];
        write_header(dir.path(), 42, &payload).await.unwrap();

        let got = read_header(dir.path(), 42, 0, 1024).await.unwrap();
        assert_eq!(got.as_deref(), Some(&payload[..]));
    }

    #[tokio::test]
    async fn read_header_partial_range_within_header() {
        let dir = tempfile::tempdir().unwrap();
        let payload: Vec<u8> = (0..200u8).collect();
        write_header(dir.path(), 7, &payload).await.unwrap();

        let got = read_header(dir.path(), 7, 50, 100).await.unwrap().unwrap();
        assert_eq!(got, payload[50..150]);
    }

    #[tokio::test]
    async fn read_header_returns_none_when_request_exceeds_header() {
        let dir = tempfile::tempdir().unwrap();
        write_header(dir.path(), 7, &vec![0u8; 64]).await.unwrap();

        // 32 + 64 = 96, past the 64-byte header.
        let got = read_header(dir.path(), 7, 32, 64).await.unwrap();
        assert!(got.is_none(), "request past header end must miss");
    }

    #[tokio::test]
    async fn read_header_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let got = read_header(dir.path(), 999, 0, 16).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn invalidate_header_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        write_header(dir.path(), 1, b"hello").await.unwrap();
        assert!(header_path(dir.path(), 1).exists());

        invalidate_header(dir.path(), 1).await;
        assert!(!header_path(dir.path(), 1).exists());

        // Calling again on a missing inode must not error.
        invalidate_header(dir.path(), 1).await;
    }

    #[tokio::test]
    async fn write_header_is_atomic_via_temp_rename() {
        // Write twice; the second write must replace the first cleanly
        // without leaving a stray temp file behind.
        let dir = tempfile::tempdir().unwrap();
        write_header(dir.path(), 5, b"first").await.unwrap();
        write_header(dir.path(), 5, b"second").await.unwrap();

        let got = read_header(dir.path(), 5, 0, 6).await.unwrap();
        assert_eq!(got.as_deref(), Some(&b"second"[..]));

        // Only the canonical header file should remain in the headers dir.
        let headers_dir = dir.path().join(".meta").join("headers");
        let entries: Vec<_> = std::fs::read_dir(&headers_dir).unwrap()
            .filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["5".to_string()]);
    }
}
