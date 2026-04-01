//! Integration tests for stratosync-core.
//!
//! These tests run against in-memory SQLite and the MockBackend, so
//! they require no cloud credentials or FUSE kernel module.
use std::sync::Arc;
use std::time::SystemTime;

use stratosync_core::{
    backend::mock::MockBackend,
    config::{parse_duration, parse_size, EvictionConfig, MountConfig, RcloneConfig},
    state::{NewFileEntry, StateDb},
    types::{FileKind, SyncStatus, FUSE_ROOT_INODE},
    Backend,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn fresh_db() -> (Arc<StateDb>, u32) {
    let db = Arc::new(StateDb::in_memory().unwrap());
    db.migrate().await.unwrap();
    let mount_id = db
        .upsert_mount("test", "local:/", "/mnt/test", "/tmp/cache", 5 << 30, 60)
        .await
        .unwrap();
    (db, mount_id)
}

fn root_entry(mount_id: u32) -> NewFileEntry {
    NewFileEntry {
        mount_id,
        parent: FUSE_ROOT_INODE,
        name: "/".into(),
        remote_path: "/".into(),
        kind: FileKind::Directory,
        size: 0,
        mtime: SystemTime::UNIX_EPOCH,
        etag: None,
        status: SyncStatus::Remote,
        cache_path: None,
        cache_size: None,
    }
}

fn file_entry(mount_id: u32, parent: u64, name: &str) -> NewFileEntry {
    NewFileEntry {
        mount_id,
        parent,
        name: name.into(),
        remote_path: format!("/{name}"),
        kind: FileKind::File,
        size: 1024,
        mtime: SystemTime::UNIX_EPOCH,
        etag: Some("etag-abc123".into()),
        status: SyncStatus::Remote,
        cache_path: None,
        cache_size: None,
    }
}

// ── DB tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn db_insert_and_lookup_by_inode() {
    let (db, mid) = fresh_db().await;
    let root = db.insert_file(&root_entry(mid)).await.unwrap();
    let inode = db.insert_file(&file_entry(mid, root, "hello.txt")).await.unwrap();

    let entry = db.get_by_inode(inode).await.unwrap().expect("entry must exist");
    assert_eq!(entry.name, "hello.txt");
    assert_eq!(entry.status, SyncStatus::Remote);
    assert_eq!(entry.size, 1024);
    assert_eq!(entry.etag.as_deref(), Some("etag-abc123"));
}

#[tokio::test]
async fn db_lookup_by_parent_and_name() {
    let (db, mid) = fresh_db().await;
    let root = db.insert_file(&root_entry(mid)).await.unwrap();
    db.insert_file(&file_entry(mid, root, "alpha.rs")).await.unwrap();
    db.insert_file(&file_entry(mid, root, "beta.rs")).await.unwrap();

    let entry = db.get_by_parent_name(root, "alpha.rs").await.unwrap().unwrap();
    assert_eq!(entry.name, "alpha.rs");

    let missing = db.get_by_parent_name(root, "gamma.rs").await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn db_list_children_ordered() {
    let (db, mid) = fresh_db().await;
    let root = db.insert_file(&root_entry(mid)).await.unwrap();
    for name in ["c.rs", "a.rs", "b.rs"] {
        db.insert_file(&file_entry(mid, root, name)).await.unwrap();
    }
    let children = db.list_children(root).await.unwrap();
    // Filter to just the files we inserted (root may also appear as its own child
    // when root.inode == FUSE_ROOT_INODE since parent_inode = FUSE_ROOT_INODE)
    let mut names: Vec<&str> = children.iter()
        .map(|e| e.name.as_str())
        .filter(|n| n.ends_with(".rs"))
        .collect();
    names.sort();
    assert_eq!(names, ["a.rs", "b.rs", "c.rs"]);
}

#[tokio::test]
async fn db_status_transitions() {
    let (db, mid) = fresh_db().await;
    let root = db.insert_file(&root_entry(mid)).await.unwrap();
    let inode = db.insert_file(&file_entry(mid, root, "notes.md")).await.unwrap();

    // Remote → Hydrating → Cached
    db.set_status(inode, SyncStatus::Hydrating).await.unwrap();
    let e = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(e.status, SyncStatus::Hydrating);

    let cache_path = std::path::Path::new("/tmp/cache/notes.md");
    db.set_cached(inode, cache_path, 512, Some("new-etag"), SystemTime::now(), 512)
        .await
        .unwrap();
    let e = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(e.status, SyncStatus::Cached);
    assert_eq!(e.cache_size, Some(512));
    assert_eq!(e.etag.as_deref(), Some("new-etag"));

    // Cached → Dirty
    db.set_status(inode, SyncStatus::Dirty).await.unwrap();
    let e = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(e.status, SyncStatus::Dirty);
}

#[tokio::test]
async fn db_eviction_clears_cache_fields() {
    let (db, mid) = fresh_db().await;
    let root = db.insert_file(&root_entry(mid)).await.unwrap();
    let inode = db.insert_file(&file_entry(mid, root, "big.tar")).await.unwrap();

    let cache_path = std::path::Path::new("/tmp/cache/big.tar");
    db.set_cached(inode, cache_path, 1 << 20, Some("e"), SystemTime::now(), 1 << 20)
        .await.unwrap();
    db.set_evicted(inode).await.unwrap();

    let e = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(e.status, SyncStatus::Remote);
    assert!(e.cache_path.is_none());
    assert!(e.cache_size.is_none());
}

#[tokio::test]
async fn db_reset_hydrating_on_restart() {
    let (db, mid) = fresh_db().await;
    let root = db.insert_file(&root_entry(mid)).await.unwrap();

    // Simulate crash: two files left in Hydrating
    for name in ["x.bin", "y.bin"] {
        let inode = db.insert_file(&file_entry(mid, root, name)).await.unwrap();
        db.set_status(inode, SyncStatus::Hydrating).await.unwrap();
    }

    let reset = db.reset_hydrating().await.unwrap();
    assert_eq!(reset, 2);

    // Both should now be Remote
    let children = db.list_children(root).await.unwrap();
    for c in children {
        assert_eq!(c.status, SyncStatus::Remote, "{} should be Remote", c.name);
    }
}

#[tokio::test]
async fn db_rename_and_delete_entry() {
    let (db, mid) = fresh_db().await;
    let root = db.insert_file(&root_entry(mid)).await.unwrap();
    let inode = db.insert_file(&file_entry(mid, root, "old.txt")).await.unwrap();

    db.rename_entry(inode, root, "new.txt", "/new.txt").await.unwrap();
    let e = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(e.name, "new.txt");
    assert_eq!(e.remote_path, "/new.txt");

    db.delete_entry(inode).await.unwrap();
    assert!(db.get_by_inode(inode).await.unwrap().is_none());
}

#[tokio::test]
async fn db_upsert_remote_file_preserves_dirty() {
    let (db, mid) = fresh_db().await;
    let root = db.insert_file(&root_entry(mid)).await.unwrap();
    let inode = db.insert_file(&file_entry(mid, root, "work.rs")).await.unwrap();

    // Locally dirty — remote poll should NOT reset to stale
    db.set_status(inode, SyncStatus::Dirty).await.unwrap();

    db.upsert_remote_file(
        mid, root, "work.rs", "/work.rs",
        FileKind::File, 2048, SystemTime::now(), Some("new-remote-etag"),
    ).await.unwrap();

    let e = db.get_by_inode(inode).await.unwrap().unwrap();
    // Dirty must be preserved
    assert_eq!(e.status, SyncStatus::Dirty, "dirty must survive remote poll");
}

#[tokio::test]
async fn db_lru_eviction_candidates_skips_pinned() {
    let (db, mid) = fresh_db().await;
    let root = db.insert_file(&root_entry(mid)).await.unwrap();
    let quota = 100u64;

    let inode_a = db.insert_file(&file_entry(mid, root, "a.bin")).await.unwrap();
    let inode_b = db.insert_file(&file_entry(mid, root, "b.bin")).await.unwrap();
    let inode_c = db.insert_file(&file_entry(mid, root, "c.bin")).await.unwrap();

    // Cache all three
    for (inode, name) in [(inode_a, "a.bin"), (inode_b, "b.bin"), (inode_c, "c.bin")] {
        let p = std::path::PathBuf::from(format!("/tmp/{name}"));
        db.set_cached(inode, &p, 30, Some("e"), SystemTime::now(), 30).await.unwrap();
    }

    // Pin b
    {
        let conn = db.raw_conn().await;
        conn.execute(
            "UPDATE cache_lru SET pinned=1 WHERE inode=?1",
            rusqlite::params![inode_b as i64],
        ).unwrap();
    }

    let candidates = db.lru_eviction_candidates(mid, 10).await.unwrap();
    let candidate_inodes: Vec<u64> = candidates.iter().map(|c| c.inode).collect();

    // b (pinned) must not appear
    assert!(!candidate_inodes.contains(&inode_b), "pinned inode must not be eviction candidate");
    assert!(candidate_inodes.contains(&inode_a));
    assert!(candidate_inodes.contains(&inode_c));
}

#[tokio::test]
async fn db_migration_idempotent_thrice() {
    let db = StateDb::in_memory().unwrap();
    for _ in 0..3 {
        db.migrate().await.expect("migration must be idempotent");
    }
}

// ── Config tests ──────────────────────────────────────────────────────────────

#[test]
fn config_parse_size_all_units() {
    assert_eq!(parse_size("1").unwrap(), 1);
    assert_eq!(parse_size("1 B").unwrap(), 1);
    assert_eq!(parse_size("4 KiB").unwrap(), 4096);
    assert_eq!(parse_size("1 MiB").unwrap(), 1 << 20);
    assert_eq!(parse_size("2 GiB").unwrap(), 2 << 30);
    assert_eq!(parse_size("1 TiB").unwrap(), 1u64 << 40);
    assert_eq!(parse_size("1 KB").unwrap(), 1024);
    assert_eq!(parse_size("1 MB").unwrap(), 1 << 20);
    assert_eq!(parse_size("1 GB").unwrap(), 1 << 30);
}

#[test]
fn config_parse_duration_all_units() {
    use std::time::Duration;
    assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
    assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
    assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
    assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    assert_eq!(parse_duration("60").unwrap(), Duration::from_secs(60));
}

#[test]
fn config_mount_cache_quota_and_poll() {
    let m = MountConfig {
        name: "test".into(),
        remote: "gdrive:/".into(),
        mount_path: "/mnt/gdrive".into(),
        cache_quota: "10 GiB".into(),
        poll_interval: "30s".into(),
        enabled: true,
        rclone: RcloneConfig::default(),
        eviction: EvictionConfig::default(),
    };
    assert_eq!(m.cache_quota_bytes().unwrap(), 10u64 * (1 << 30));
    assert_eq!(m.poll_duration().unwrap().as_secs(), 30);
}

// ── Backend mock tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn mock_backend_seed_and_stat() {
    let backend = MockBackend::default();
    backend.seed_file("/doc.pdf", b"hello world");

    let meta = backend.stat("/doc.pdf").await.unwrap();
    assert_eq!(meta.name, "doc.pdf");
    assert_eq!(meta.size, 11);
    assert!(meta.etag.is_some());
}

#[tokio::test]
async fn mock_backend_download_to_file() {
    let backend = MockBackend::default();
    backend.seed_file("/data.bin", b"test content");

    let tmp = tempfile_path();
    backend.download("/data.bin", &tmp).await.unwrap();

    let content = std::fs::read(&tmp).unwrap();
    assert_eq!(content, b"test content");
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn mock_backend_upload_and_stat() {
    use stratosync_core::Backend;
    let backend = MockBackend::default();

    let tmp = tempfile_path();
    std::fs::write(&tmp, b"uploaded content").unwrap();

    let meta = backend.upload(&tmp, "/uploaded.txt", None).await.unwrap();
    assert_eq!(meta.name, "uploaded.txt");
    assert_eq!(meta.size, 16);

    let stat = backend.stat("/uploaded.txt").await.unwrap();
    assert_eq!(stat.size, 16);

    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn mock_backend_delete() {
    use stratosync_core::types::SyncError;
    let backend = MockBackend::default();
    backend.seed_file("/todelete.txt", b"bye");

    backend.delete("/todelete.txt").await.unwrap();

    // Should now return NotFound
    let err = backend.stat("/todelete.txt").await.unwrap_err();
    assert!(matches!(err, SyncError::NotFound(_)));
}

#[tokio::test]
async fn mock_backend_rename() {
    let backend = MockBackend::default();
    backend.seed_file("/old.txt", b"content");

    backend.rename("/old.txt", "/new.txt").await.unwrap();

    assert!(backend.stat("/new.txt").await.is_ok());
    assert!(backend.stat("/old.txt").await.is_err());
}

#[tokio::test]
async fn mock_backend_fail_on() {
    let backend = MockBackend::default();
    backend.seed_file("/fragile.txt", b"data");
    backend.fail_on("/fragile.txt");

    let result = backend.stat("/fragile.txt").await;
    assert!(result.is_err());
}

// ── SyncStatus helpers ────────────────────────────────────────────────────────

#[test]
fn sync_status_has_local_data() {
    assert!(SyncStatus::Cached.has_local_data());
    assert!(SyncStatus::Dirty.has_local_data());
    assert!(SyncStatus::Uploading.has_local_data());
    assert!(!SyncStatus::Remote.has_local_data());
    assert!(!SyncStatus::Stale.has_local_data());
    assert!(!SyncStatus::Hydrating.has_local_data());
}

#[test]
fn sync_status_needs_hydration() {
    assert!(SyncStatus::Remote.needs_hydration());
    assert!(SyncStatus::Stale.needs_hydration());
    assert!(!SyncStatus::Cached.needs_hydration());
    assert!(!SyncStatus::Dirty.needs_hydration());
}

#[test]
fn sync_status_roundtrip() {
    for s in [
        SyncStatus::Remote, SyncStatus::Hydrating, SyncStatus::Cached,
        SyncStatus::Dirty,  SyncStatus::Uploading,  SyncStatus::Stale,
        SyncStatus::Conflict,
    ] {
        let roundtripped = SyncStatus::from_str(s.as_str()).unwrap();
        assert_eq!(roundtripped, s);
    }
}

#[test]
fn file_entry_stem_and_extension() {
    use stratosync_core::types::FileEntry;
    let e = FileEntry {
        inode: 1, mount_id: 1, parent: 1,
        name: "report.conflict.20250101T000000Z.abcd1234.pdf".into(),
        remote_path: "/report.pdf".into(),
        kind: FileKind::File,
        size: 0, mtime: SystemTime::UNIX_EPOCH,
        etag: None, status: SyncStatus::Cached,
        cache_path: None, cache_size: None, dir_listed: None,
    };
    assert_eq!(e.extension(), Some("pdf"));
    assert_eq!(e.stem(), "report.conflict.20250101T000000Z.abcd1234");
}

// ── Upload queue / retry ─────────────────────────────────────────────────────

#[tokio::test]
async fn db_enqueue_and_dequeue_upload() {
    let (db, mid) = fresh_db().await;
    let root = db.insert_file(&root_entry(mid)).await.unwrap();
    let inode = db.insert_file(&file_entry(mid, root, "todo.txt")).await.unwrap();

    db.enqueue_upload(inode, mid, "/todo.txt", Some("etag-old"), 1).await.unwrap();

    let job = db.dequeue_next_upload(mid).await.unwrap();
    assert!(job.is_some());
    let job = job.unwrap();
    assert_eq!(job.inode, inode);
    assert_eq!(job.remote_path, "/todo.txt");
    assert_eq!(job.known_etag.as_deref(), Some("etag-old"));

    // Complete it — should be gone
    db.complete_queue_job(job.id).await.unwrap();
    let next = db.dequeue_next_upload(mid).await.unwrap();
    assert!(next.is_none());
}

#[tokio::test]
async fn db_fail_queue_job_applies_backoff() {
    let (db, mid) = fresh_db().await;
    let root = db.insert_file(&root_entry(mid)).await.unwrap();
    let inode = db.insert_file(&file_entry(mid, root, "slow.bin")).await.unwrap();
    db.set_status(inode, SyncStatus::Dirty).await.unwrap();

    db.enqueue_upload(inode, mid, "/slow.bin", None, 1).await.unwrap();
    let job = db.dequeue_next_upload(mid).await.unwrap().unwrap();

    db.fail_queue_job(job.id, "network error", 30).await.unwrap();

    // Should not be dequeued again immediately (next_attempt is in the future)
    let next = db.dequeue_next_upload(mid).await.unwrap();
    assert!(next.is_none(), "job with future next_attempt must not be dequeued");
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn tempfile_path() -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH).unwrap()
        .subsec_nanos();
    std::path::PathBuf::from(format!("/tmp/stratosync_test_{ts}"))
}
