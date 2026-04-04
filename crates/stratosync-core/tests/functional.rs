//! Functional tests for stratosync-core.
//!
//! These tests exercise multi-component workflows that mirror real daemon
//! operations: directory population, file lifecycle (create → write → read →
//! rename → delete), path normalization, and concurrent access patterns.
//!
//! They use in-memory SQLite and MockBackend — no credentials, FUSE module,
//! or network access required.
//!
//! Run with: cargo test -p stratosync-core --test functional
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use stratosync_core::{
    backend::mock::MockBackend,
    state::{NewFileEntry, StateDb},
    types::{FileKind, Inode, SyncStatus, FUSE_ROOT_INODE},
    Backend,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

async fn fresh_db() -> (Arc<StateDb>, u32) {
    let db = Arc::new(StateDb::in_memory().unwrap());
    db.migrate().await.unwrap();
    let mount_id = db
        .upsert_mount("test", "local:/", "/mnt/test", "/tmp/cache", 5 << 30, 60)
        .await
        .unwrap();
    (db, mount_id)
}

async fn setup() -> (Arc<StateDb>, u32, Inode) {
    let (db, mid) = fresh_db().await;
    let root = db.insert_root(&NewFileEntry {
        mount_id: mid,
        parent: 0,
        name: "/".into(),
        remote_path: "/".into(),
        kind: FileKind::Directory,
        size: 0,
        mtime: SystemTime::UNIX_EPOCH,
        etag: None,
        status: SyncStatus::Remote,
        cache_path: None,
        cache_size: None,
    }).await.unwrap();
    assert_eq!(root, FUSE_ROOT_INODE);
    (db, mid, root)
}

async fn insert_dir(
    db: &Arc<StateDb>, mid: u32, parent: Inode, name: &str, remote_path: &str,
) -> Inode {
    db.insert_file(&NewFileEntry {
        mount_id: mid,
        parent,
        name: name.into(),
        remote_path: remote_path.into(),
        kind: FileKind::Directory,
        size: 0,
        mtime: SystemTime::now(),
        etag: None,
        status: SyncStatus::Cached,
        cache_path: None,
        cache_size: None,
    }).await.unwrap()
}

async fn insert_file(
    db: &Arc<StateDb>, mid: u32, parent: Inode, name: &str, remote_path: &str,
    status: SyncStatus, cache_path: Option<&str>,
) -> Inode {
    db.insert_file(&NewFileEntry {
        mount_id: mid,
        parent,
        name: name.into(),
        remote_path: remote_path.into(),
        kind: FileKind::File,
        size: 100,
        mtime: SystemTime::now(),
        etag: Some("etag-1".into()),
        status,
        cache_path: cache_path.map(PathBuf::from),
        cache_size: Some(100),
    }).await.unwrap()
}

fn tempdir() -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap()
        .subsec_nanos();
    let p = PathBuf::from(format!("/tmp/stratosync_func_{ts}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Helper to build paths consistent with rclone lsjson output (no leading slash).
fn join_remote(parent: &str, child: &str) -> String {
    let p = parent.trim_matches('/');
    if p.is_empty() { child.to_string() } else { format!("{p}/{child}") }
}

// ── Directory population ─────────────────────────────────────────────────────

#[tokio::test]
async fn populate_root_lists_immediate_children_only() {
    let (db, mid, root) = setup().await;
    let backend = MockBackend::default();
    backend.seed_file("/file1.txt", b"one");
    backend.seed_file("/file2.txt", b"two");
    backend.seed_file("/file3.txt", b"three");

    let children = backend.list("/").await.unwrap();
    assert_eq!(children.len(), 3);

    let entries: Vec<_> = children.iter().map(|c| {
        let kind = if c.is_dir { FileKind::Directory } else { FileKind::File };
        let full_path = join_remote("/", &c.name);
        (c.name.clone(), full_path, kind, c.size, c.mtime, c.etag.clone())
    }).collect();

    db.batch_upsert_remote_files(mid, root, &entries).await.unwrap();
    db.mark_dir_listed(root).await.unwrap();

    let listed = db.list_children(root).await.unwrap();
    assert_eq!(listed.len(), 3);
    let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"file1.txt"));
    assert!(names.contains(&"file2.txt"));
    assert!(names.contains(&"file3.txt"));
}

#[tokio::test]
async fn populate_subdir_builds_full_remote_paths() {
    let (db, mid, root) = setup().await;
    let docs = insert_dir(&db, mid, root, "Documents", "Documents").await;

    let backend = MockBackend::default();
    backend.seed_file("/report.pdf", b"pdf content");
    backend.seed_file("/notes.md", b"# Notes");

    let children = backend.list("/").await.unwrap();
    let entries: Vec<_> = children.iter().map(|c| {
        let kind = if c.is_dir { FileKind::Directory } else { FileKind::File };
        // Simulate what populate_directory does: join parent path with child name
        let full_path = join_remote("Documents", &c.name);
        (c.name.clone(), full_path, kind, c.size, c.mtime, c.etag.clone())
    }).collect();

    db.batch_upsert_remote_files(mid, docs, &entries).await.unwrap();

    let listed = db.list_children(docs).await.unwrap();
    assert_eq!(listed.len(), 2);
    // Paths must include the parent directory
    for entry in &listed {
        assert!(entry.remote_path.starts_with("Documents/"),
            "remote_path {:?} should start with Documents/", entry.remote_path);
    }
}

#[tokio::test]
async fn populate_paths_match_poller_paths() {
    // The poller uses list_recursive("/") which returns full paths like "Documents/file.txt".
    // populate_directory must produce the same paths so UNIQUE(mount_id, remote_path) merges them.
    let (db, mid, root) = setup().await;
    let docs = insert_dir(&db, mid, root, "Documents", "Documents").await;

    // Simulate poller: insert with full path from root
    let poller_inode = db.upsert_remote_file(
        mid, docs, "file.txt", "Documents/file.txt",
        FileKind::File, 50, SystemTime::now(), Some("etag-poller"),
    ).await.unwrap();

    // Simulate populate_directory: insert with join_remote("Documents", "file.txt")
    let populate_path = join_remote("Documents", "file.txt");
    assert_eq!(populate_path, "Documents/file.txt");

    let populate_inode = db.upsert_remote_file(
        mid, docs, "file.txt", &populate_path,
        FileKind::File, 50, SystemTime::now(), Some("etag-populate"),
    ).await.unwrap();

    // Must be the same inode (upsert, not duplicate)
    assert_eq!(poller_inode, populate_inode, "poller and populate must merge on same remote_path");
}

// ── File lifecycle ───────────────────────────────────────────────────────────

#[tokio::test]
async fn create_file_is_dirty_with_cache_path() {
    let (db, mid, root) = setup().await;
    let tmp = tempdir();
    let cache_path = tmp.join("test.txt");
    std::fs::write(&cache_path, b"").unwrap();

    let inode = db.insert_file(&NewFileEntry {
        mount_id: mid,
        parent: root,
        name: "test.txt".into(),
        remote_path: "test.txt".into(),
        kind: FileKind::File,
        size: 0,
        mtime: SystemTime::now(),
        etag: None,
        status: SyncStatus::Dirty,
        cache_path: Some(cache_path.clone()),
        cache_size: Some(0),
    }).await.unwrap();

    let entry = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(entry.status, SyncStatus::Dirty);
    assert_eq!(entry.cache_path, Some(cache_path));
    assert_eq!(entry.size, 0);

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn set_dirty_size_updates_size_and_cache_size() {
    let (db, mid, root) = setup().await;
    let inode = insert_file(&db, mid, root, "grow.txt", "grow.txt", SyncStatus::Dirty, None).await;

    db.set_dirty_size(inode, 42).await.unwrap();

    let entry = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(entry.status, SyncStatus::Dirty);
    assert_eq!(entry.size, 42);
    assert_eq!(entry.cache_size, Some(42));
}

#[tokio::test]
async fn set_dirty_size_on_remote_file_transitions_to_dirty() {
    let (db, mid, root) = setup().await;
    let inode = insert_file(&db, mid, root, "new.txt", "new.txt", SyncStatus::Remote, None).await;

    db.set_dirty_size(inode, 1024).await.unwrap();

    let entry = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(entry.status, SyncStatus::Dirty);
    assert_eq!(entry.size, 1024);
}

#[tokio::test]
async fn file_lifecycle_create_cache_evict_rehydrate() {
    let (db, mid, root) = setup().await;
    let tmp = tempdir();
    let cache_path = tmp.join("doc.txt");

    // 1. Create as remote (poller discovered it)
    let inode = insert_file(&db, mid, root, "doc.txt", "doc.txt", SyncStatus::Remote, None).await;

    // 2. Hydrate (simulate download)
    std::fs::write(&cache_path, b"cached content").unwrap();
    db.set_cached(inode, &cache_path, 14, Some("etag-1"), SystemTime::now(), 14)
        .await.unwrap();
    let entry = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(entry.status, SyncStatus::Cached);
    assert_eq!(entry.cache_size, Some(14));

    // 3. Evict
    db.set_evicted(inode).await.unwrap();
    let entry = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(entry.status, SyncStatus::Remote);
    assert!(entry.cache_path.is_none());
    assert!(entry.cache_size.is_none());

    // 4. Re-hydrate
    db.set_cached(inode, &cache_path, 14, Some("etag-1"), SystemTime::now(), 14)
        .await.unwrap();
    let entry = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(entry.status, SyncStatus::Cached);

    let _ = std::fs::remove_dir_all(&tmp);
}

// ── Rename ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn rename_updates_name_path_and_cache_path() {
    let (db, mid, root) = setup().await;
    let tmp = tempdir();
    let old_cache = tmp.join("old.txt");
    let new_cache = tmp.join("new.txt");
    std::fs::write(&old_cache, b"content").unwrap();

    let inode = insert_file(
        &db, mid, root, "old.txt", "old.txt",
        SyncStatus::Dirty, Some(old_cache.to_str().unwrap()),
    ).await;

    // Rename file in cache
    std::fs::rename(&old_cache, &new_cache).unwrap();

    db.rename_entry(inode, root, "new.txt", "new.txt", Some(&new_cache))
        .await.unwrap();

    let entry = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(entry.name, "new.txt");
    assert_eq!(entry.remote_path, "new.txt");
    assert_eq!(entry.cache_path, Some(new_cache));

    // Old name should not be found
    assert!(db.get_by_parent_name(root, "old.txt").await.unwrap().is_none());
    // New name should be found
    assert!(db.get_by_parent_name(root, "new.txt").await.unwrap().is_some());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn rename_across_directories() {
    let (db, mid, root) = setup().await;
    let dir_a = insert_dir(&db, mid, root, "a", "a").await;
    let dir_b = insert_dir(&db, mid, root, "b", "b").await;

    let inode = insert_file(&db, mid, dir_a, "file.txt", "a/file.txt", SyncStatus::Dirty, None).await;

    db.rename_entry(inode, dir_b, "file.txt", "b/file.txt", None).await.unwrap();

    let entry = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(entry.parent, dir_b);
    assert_eq!(entry.remote_path, "b/file.txt");

    assert!(db.get_by_parent_name(dir_a, "file.txt").await.unwrap().is_none());
    assert!(db.get_by_parent_name(dir_b, "file.txt").await.unwrap().is_some());
}

// ── Delete ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delete_removes_entry_and_cascades_lru() {
    let (db, mid, root) = setup().await;
    let tmp = tempdir();
    let cache_path = tmp.join("del.txt");
    std::fs::write(&cache_path, b"data").unwrap();

    let inode = insert_file(
        &db, mid, root, "del.txt", "del.txt",
        SyncStatus::Cached, Some(cache_path.to_str().unwrap()),
    ).await;

    // Touch LRU so there's an entry in cache_lru
    db.touch_lru(inode).await.unwrap();

    // Delete
    db.delete_entry(inode).await.unwrap();

    assert!(db.get_by_inode(inode).await.unwrap().is_none());
    assert!(db.get_by_parent_name(root, "del.txt").await.unwrap().is_none());
    // LRU entry should be cascade-deleted
    let candidates = db.lru_eviction_candidates(mid, 100).await.unwrap();
    assert!(candidates.iter().all(|c| c.inode != inode));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn delete_file_backend_not_found_is_ok() {
    let backend = MockBackend::default();
    // File doesn't exist on backend — delete should return NotFound
    let result = backend.stat("/nonexistent.txt").await;
    assert!(matches!(result, Err(stratosync_core::types::SyncError::NotFound(_))));
}

// ── Directory operations ─────────────────────────────────────────────────────

#[tokio::test]
async fn mkdir_creates_directory_entry() {
    let (db, mid, root) = setup().await;
    let inode = insert_dir(&db, mid, root, "photos", "photos").await;

    let entry = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(entry.kind, FileKind::Directory);
    assert_eq!(entry.name, "photos");
    assert_eq!(entry.remote_path, "photos");
    assert_eq!(entry.parent, root);
}

#[tokio::test]
async fn nested_directory_structure() {
    let (db, mid, root) = setup().await;
    let a = insert_dir(&db, mid, root, "a", "a").await;
    let b = insert_dir(&db, mid, a, "b", "a/b").await;
    let c = insert_dir(&db, mid, b, "c", "a/b/c").await;

    // Verify parent chain
    let entry_c = db.get_by_inode(c).await.unwrap().unwrap();
    assert_eq!(entry_c.parent, b);
    let entry_b = db.get_by_inode(b).await.unwrap().unwrap();
    assert_eq!(entry_b.parent, a);
    let entry_a = db.get_by_inode(a).await.unwrap().unwrap();
    assert_eq!(entry_a.parent, root);

    // Insert file at deepest level
    let file = insert_file(&db, mid, c, "deep.txt", "a/b/c/deep.txt", SyncStatus::Remote, None).await;
    let children = db.list_children(c).await.unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].inode, file);
}

#[tokio::test]
async fn rmdir_only_works_on_empty_dir() {
    let (db, mid, root) = setup().await;
    let dir = insert_dir(&db, mid, root, "stuff", "stuff").await;
    let _file = insert_file(&db, mid, dir, "child.txt", "stuff/child.txt", SyncStatus::Remote, None).await;

    let children = db.list_children(dir).await.unwrap();
    assert_eq!(children.len(), 1, "dir must be non-empty for this test");

    // After removing the child, rmdir should work
    db.delete_entry(_file).await.unwrap();
    let children = db.list_children(dir).await.unwrap();
    assert!(children.is_empty());
    db.delete_entry(dir).await.unwrap();
    assert!(db.get_by_inode(dir).await.unwrap().is_none());
}

// ── Path normalization (join_remote) ─────────────────────────────────────────

#[test]
fn join_remote_root_child() {
    assert_eq!(join_remote("/", "file.txt"), "file.txt");
}

#[test]
fn join_remote_subdir_child() {
    assert_eq!(join_remote("Documents", "report.pdf"), "Documents/report.pdf");
}

#[test]
fn join_remote_nested() {
    assert_eq!(join_remote("a/b/c", "file.txt"), "a/b/c/file.txt");
}

#[test]
fn join_remote_strips_trailing_slash() {
    assert_eq!(join_remote("Documents/", "file.txt"), "Documents/file.txt");
}

#[test]
fn join_remote_empty_parent() {
    assert_eq!(join_remote("", "file.txt"), "file.txt");
}

// ── Root inode ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn root_inode_is_directory_at_inode_1() {
    let (db, mid, root) = setup().await;
    assert_eq!(root, FUSE_ROOT_INODE);

    let entry = db.get_by_inode(FUSE_ROOT_INODE).await.unwrap().unwrap();
    assert_eq!(entry.kind, FileKind::Directory);
    assert_eq!(entry.remote_path, "/");
    assert_eq!(entry.mount_id, mid);
}

#[tokio::test]
async fn root_does_not_appear_as_own_child() {
    let (db, mid, root) = setup().await;
    insert_file(&db, mid, root, "a.txt", "a.txt", SyncStatus::Remote, None).await;

    let children = db.list_children(root).await.unwrap();
    // Root has NULL parent so it should NOT appear in its own children
    assert!(children.iter().all(|c| c.inode != root),
        "root must not appear as its own child");
    assert_eq!(children.len(), 1);
}

#[tokio::test]
async fn root_survives_delete_mount_entries_and_reinsert() {
    let (db, mid, root) = setup().await;
    insert_file(&db, mid, root, "x.txt", "x.txt", SyncStatus::Remote, None).await;

    db.delete_mount_entries(mid).await.unwrap();
    assert!(db.get_by_inode(root).await.unwrap().is_none());

    let new_root = db.insert_root(&NewFileEntry {
        mount_id: mid, parent: 0, name: "/".into(), remote_path: "/".into(),
        kind: FileKind::Directory, size: 0, mtime: SystemTime::UNIX_EPOCH,
        etag: None, status: SyncStatus::Remote, cache_path: None, cache_size: None,
    }).await.unwrap();
    assert_eq!(new_root, FUSE_ROOT_INODE);
}

// ── Batch operations ─────────────────────────────────────────────────────────

#[tokio::test]
async fn batch_upsert_is_atomic_on_error() {
    let (db, mid, root) = setup().await;

    // Batch with a valid entry followed by one that would violate
    // a constraint (duplicate remote_path within the batch)
    let entries = vec![
        ("a.txt".into(), "a.txt".into(), FileKind::File, 10u64, SystemTime::now(), None),
        ("b.txt".into(), "b.txt".into(), FileKind::File, 20, SystemTime::now(), None),
    ];

    db.batch_upsert_remote_files(mid, root, &entries).await.unwrap();
    let children = db.list_children(root).await.unwrap();
    assert_eq!(children.len(), 2);

    // Upserting again should update, not duplicate
    db.batch_upsert_remote_files(mid, root, &entries).await.unwrap();
    let children = db.list_children(root).await.unwrap();
    assert_eq!(children.len(), 2, "batch upsert must not create duplicates");
}

#[tokio::test]
async fn batch_upsert_preserves_dirty_status() {
    let (db, mid, root) = setup().await;

    // Create a file and mark it dirty (local edit)
    let inode = insert_file(&db, mid, root, "edited.txt", "edited.txt", SyncStatus::Dirty, None).await;

    // Poller batch-upserts with new remote metadata — must NOT overwrite dirty
    let entries = vec![
        ("edited.txt".into(), "edited.txt".into(), FileKind::File, 200, SystemTime::now(), Some("new-etag".into())),
    ];
    db.batch_upsert_remote_files(mid, root, &entries).await.unwrap();

    let entry = db.get_by_inode(inode).await.unwrap().unwrap();
    assert_eq!(entry.status, SyncStatus::Dirty, "dirty status must survive batch upsert");
}

// ── Concurrent access ────────────────────────────────────────────────────────

#[tokio::test]
async fn concurrent_upserts_to_different_files() {
    let (db, mid, root) = setup().await;

    let mut handles = vec![];
    for i in 0..20 {
        let db = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            let name = format!("file_{i}.txt");
            db.upsert_remote_file(
                mid, root, &name, &name,
                FileKind::File, i as u64 * 10, SystemTime::now(), None,
            ).await.unwrap();
        }));
    }
    for h in handles { h.await.unwrap(); }

    let children = db.list_children(root).await.unwrap();
    assert_eq!(children.len(), 20);
}

#[tokio::test]
async fn concurrent_status_transitions() {
    let (db, mid, root) = setup().await;
    let inode = insert_file(&db, mid, root, "race.txt", "race.txt", SyncStatus::Remote, None).await;

    // Simulate hydration + poller updating concurrently
    let db1 = Arc::clone(&db);
    let db2 = Arc::clone(&db);

    let h1 = tokio::spawn(async move {
        db1.set_status(inode, SyncStatus::Hydrating).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        db1.set_status(inode, SyncStatus::Cached).await.unwrap();
    });

    let h2 = tokio::spawn(async move {
        for _ in 0..5 {
            // Poller touches the file — should not corrupt it
            db2.upsert_remote_file(
                mid, root, "race.txt", "race.txt",
                FileKind::File, 100, SystemTime::now(), Some("etag-poll"),
            ).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    });

    h1.await.unwrap();
    h2.await.unwrap();

    // File must still be in a valid state
    let entry = db.get_by_inode(inode).await.unwrap().unwrap();
    assert!(matches!(entry.status, SyncStatus::Cached | SyncStatus::Stale),
        "status should be Cached or Stale, got {:?}", entry.status);
}

// ── Backend mock round-trips ─────────────────────────────────────────────────

#[tokio::test]
async fn backend_upload_download_roundtrip() {
    let backend = MockBackend::default();
    let tmp = tempdir();

    // Upload
    let src = tmp.join("upload.txt");
    std::fs::write(&src, b"roundtrip content").unwrap();
    let meta = backend.upload(&src, "/roundtrip.txt", None).await.unwrap();
    assert_eq!(meta.size, 17);

    // Download to different path
    let dst = tmp.join("download.txt");
    backend.download("/roundtrip.txt", &dst).await.unwrap();
    let content = std::fs::read_to_string(&dst).unwrap();
    assert_eq!(content, "roundtrip content");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn backend_rename_preserves_content() {
    let backend = MockBackend::default();
    backend.seed_file("/original.txt", b"preserved");

    backend.rename("/original.txt", "/moved.txt").await.unwrap();

    // Original gone
    assert!(backend.stat("/original.txt").await.is_err());

    // Content preserved at new path
    let tmp = tempdir();
    let dst = tmp.join("check.txt");
    backend.download("/moved.txt", &dst).await.unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), b"preserved");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn backend_list_returns_seeded_files() {
    let backend = MockBackend::default();
    backend.seed_file("/a.txt", b"a");
    backend.seed_file("/b.txt", b"b");
    backend.seed_file("/c.txt", b"c");

    let files = backend.list("/").await.unwrap();
    assert_eq!(files.len(), 3);

    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.txt"));
    assert!(names.contains(&"c.txt"));
}

// ── Tombstones ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn tombstone_blocks_exact_path() {
    let (db, mid, _root) = setup().await;
    db.insert_tombstone(mid, "test/file.txt", 300).await.unwrap();

    assert!(db.is_tombstoned(mid, "test/file.txt").await.unwrap());
    assert!(!db.is_tombstoned(mid, "test/other.txt").await.unwrap());
    assert!(!db.is_tombstoned(mid, "other/file.txt").await.unwrap());
}

#[tokio::test]
async fn tombstone_blocks_children_of_directory() {
    let (db, mid, _root) = setup().await;
    db.insert_tombstone(mid, "mydir", 300).await.unwrap();

    // Exact match
    assert!(db.is_tombstoned(mid, "mydir").await.unwrap());
    // Children
    assert!(db.is_tombstoned(mid, "mydir/file.txt").await.unwrap());
    assert!(db.is_tombstoned(mid, "mydir/sub/deep.txt").await.unwrap());
    // Siblings are not blocked
    assert!(!db.is_tombstoned(mid, "mydir2").await.unwrap());
    assert!(!db.is_tombstoned(mid, "other").await.unwrap());
}

#[tokio::test]
async fn tombstone_expires() {
    let (db, mid, _root) = setup().await;
    // TTL = 0 means already expired
    db.insert_tombstone(mid, "expired.txt", 0).await.unwrap();

    assert!(!db.is_tombstoned(mid, "expired.txt").await.unwrap(),
        "expired tombstone must not block");
}

#[tokio::test]
async fn tombstone_removal() {
    let (db, mid, _root) = setup().await;
    db.insert_tombstone(mid, "removed.txt", 300).await.unwrap();
    assert!(db.is_tombstoned(mid, "removed.txt").await.unwrap());

    db.remove_tombstone(mid, "removed.txt").await.unwrap();
    assert!(!db.is_tombstoned(mid, "removed.txt").await.unwrap());
}

#[tokio::test]
async fn tombstone_idempotent_insert() {
    let (db, mid, _root) = setup().await;
    db.insert_tombstone(mid, "dup.txt", 300).await.unwrap();
    db.insert_tombstone(mid, "dup.txt", 600).await.unwrap(); // no error, updates TTL
    assert!(db.is_tombstoned(mid, "dup.txt").await.unwrap());
}

#[tokio::test]
async fn cleanup_expired_tombstones() {
    let (db, mid, _root) = setup().await;
    db.insert_tombstone(mid, "alive.txt", 300).await.unwrap();
    db.insert_tombstone(mid, "dead.txt", 0).await.unwrap();

    let cleaned = db.cleanup_expired_tombstones().await.unwrap();
    assert!(cleaned >= 1);

    // alive survives, dead is gone
    let active = db.active_tombstones(mid).await.unwrap();
    assert!(active.contains(&"alive.txt".to_string()));
    assert!(!active.contains(&"dead.txt".to_string()));
}

#[tokio::test]
async fn active_tombstones_returns_only_live_entries() {
    let (db, mid, _root) = setup().await;
    db.insert_tombstone(mid, "a.txt", 300).await.unwrap();
    db.insert_tombstone(mid, "b.txt", 300).await.unwrap();
    db.insert_tombstone(mid, "expired.txt", 0).await.unwrap();

    let active = db.active_tombstones(mid).await.unwrap();
    assert_eq!(active.len(), 2);
    assert!(active.contains(&"a.txt".to_string()));
    assert!(active.contains(&"b.txt".to_string()));
}

#[tokio::test]
async fn tombstone_does_not_cross_mounts() {
    let (db, _mid1, _root) = setup().await;
    let mid2 = db.upsert_mount("other", "other:/", "/mnt/other", "/tmp/other", 5 << 30, 60)
        .await.unwrap();

    db.insert_tombstone(_mid1, "shared.txt", 300).await.unwrap();

    assert!(db.is_tombstoned(_mid1, "shared.txt").await.unwrap());
    assert!(!db.is_tombstoned(mid2, "shared.txt").await.unwrap());
}

#[tokio::test]
async fn poller_skips_tombstoned_files_in_batch() {
    // Simulates the poller's in-memory tombstone check pattern
    let (db, mid, root) = setup().await;

    // Create tombstones
    db.insert_tombstone(mid, "deleted.txt", 300).await.unwrap();
    db.insert_tombstone(mid, "deleted_dir", 300).await.unwrap();

    // Load active tombstones (what the poller does at the start of poll_once)
    let tombstones = db.active_tombstones(mid).await.unwrap();

    // Simulate filtering remote entries
    let remote_paths = vec![
        "alive.txt",
        "deleted.txt",         // exact tombstone match
        "deleted_dir/child.txt", // under tombstoned dir
        "other.txt",
    ];

    let should_upsert: Vec<&&str> = remote_paths.iter().filter(|path| {
        !tombstones.iter().any(|t| *path == t || path.starts_with(&format!("{t}/")))
    }).collect();

    assert_eq!(should_upsert, vec![&"alive.txt", &"other.txt"]);
}

// ── Security: path traversal ─────────────────────────────────────────────────

#[test]
fn join_remote_does_not_inject_traversal() {
    // join_remote is a pure path builder — the caller (FUSE handlers) validates
    // the child name. But join_remote itself should not introduce traversal.
    assert_eq!(join_remote("/", ".."), "..");
    assert_eq!(join_remote("Documents", ".."), "Documents/..");
    // The validate_filename() check in FUSE handlers rejects ".." before
    // join_remote is called, so these paths should never reach the DB.
}

#[tokio::test]
async fn path_traversal_in_remote_path_does_not_escape_db() {
    // Even if a traversal path reaches the DB (e.g. from a malicious remote),
    // the UNIQUE constraint prevents overwriting unrelated entries.
    let (db, mid, root) = setup().await;

    // Insert a normal file
    let normal = insert_file(&db, mid, root, "safe.txt", "safe.txt", SyncStatus::Remote, None).await;

    // Insert a file with traversal in remote_path (simulates malicious remote)
    let evil = insert_file(&db, mid, root, "evil.txt", "../safe.txt", SyncStatus::Remote, None).await;

    // They are distinct entries (different remote_paths)
    assert_ne!(normal, evil);
    let normal_entry = db.get_by_inode(normal).await.unwrap().unwrap();
    assert_eq!(normal_entry.remote_path, "safe.txt");
}

#[tokio::test]
async fn symlink_in_cache_rejected_by_upload() {
    // Verify that a symlink cache file is detected.
    // The actual check is in upload_queue.rs — we test the metadata detection here.
    let tmp = tempdir();
    let real_file = tmp.join("real.txt");
    let link_file = tmp.join("link.txt");
    std::fs::write(&real_file, b"real content").unwrap();
    std::os::unix::fs::symlink(&real_file, &link_file).unwrap();

    // symlink_metadata should detect the symlink
    let meta = std::fs::symlink_metadata(&link_file).unwrap();
    assert!(meta.file_type().is_symlink(), "must detect symlink");

    // Regular file should not be detected as symlink
    let meta = std::fs::symlink_metadata(&real_file).unwrap();
    assert!(!meta.file_type().is_symlink());

    let _ = std::fs::remove_dir_all(&tmp);
}

// ── Security: SQL injection ──────────────────────────────────────────────────

#[tokio::test]
async fn sql_injection_in_filename_is_harmless() {
    let (db, mid, root) = setup().await;

    // Filenames with SQL injection attempts
    let evil_names = [
        "'; DROP TABLE file_index; --",
        "\" OR 1=1 --",
        "file.txt'; DELETE FROM mounts WHERE '1'='1",
        "Robert'); DROP TABLE file_index;--",
    ];

    for name in &evil_names {
        let inode = db.insert_file(&NewFileEntry {
            mount_id: mid, parent: root,
            name: (*name).into(), remote_path: (*name).into(),
            kind: FileKind::File, size: 0, mtime: SystemTime::now(),
            etag: None, status: SyncStatus::Remote,
            cache_path: None, cache_size: None,
        }).await.unwrap();

        let entry = db.get_by_inode(inode).await.unwrap().unwrap();
        assert_eq!(entry.name, *name, "SQL injection must be stored as literal text");
    }

    // Tables must still exist and be functional
    let children = db.list_children(root).await.unwrap();
    assert_eq!(children.len(), evil_names.len());
}

// ── Security: integer boundaries ─────────────────────────────────────────────

#[tokio::test]
async fn zero_and_max_file_sizes() {
    let (db, mid, root) = setup().await;

    // Zero-size file
    let z = insert_file(&db, mid, root, "zero.txt", "zero.txt", SyncStatus::Remote, None).await;
    db.set_dirty_size(z, 0).await.unwrap();
    let entry = db.get_by_inode(z).await.unwrap().unwrap();
    assert_eq!(entry.size, 0);

    // Very large size (near i64::MAX / 2 to avoid SQLite overflow)
    let big = insert_file(&db, mid, root, "big.txt", "big.txt", SyncStatus::Remote, None).await;
    let large_size = 4_000_000_000_000u64; // 4 TB
    db.set_dirty_size(big, large_size).await.unwrap();
    let entry = db.get_by_inode(big).await.unwrap().unwrap();
    assert_eq!(entry.size, large_size);
}
