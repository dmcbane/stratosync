//! Upload `if_match` precondition against real-shaped `lsjson --hash`
//! output, using a fake `rclone` script.
//!
//! Live failure: the stored ETag and the one rclone's `stat` returned
//! were different spellings of different hashes (gdrive: delta-stored
//! md5 vs stat's sha1; OneDrive: delta-stored sha1/quickxor vs stat's
//! item ID). Every upload became a "conflict", and an edited binary file
//! got a spurious `.conflict` copy.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use stratosync_core::{Backend, RcloneBackend, SyncError};

const LSJSON: &str = r#"[{"Path":"f.pdf","Name":"f.pdf","Size":5,"MimeType":"application/pdf","ModTime":"2024-11-13T17:38:24.583Z","IsDir":false,"Hashes":{"md5":"8cdbe53eaf2dcf885824a5c1563e2a4a","sha1":"847bfc9ae8a6ad75e03bbda5b0200194d2be6049"},"ID":"10TlMHh1Slezu3D1K0NaDI6HIJKVJLWhn"}]"#;

async fn upload_with_stored_etag(stored: &str) -> Result<(), SyncError> {
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("fake-rclone");
    std::fs::write(&script, format!(
        "#!/usr/bin/env bash\ncase \"$1\" in\n  lsjson) printf '%s' '{LSJSON}' ;;\n  *) exit 0 ;;\nesac\n"
    )).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let local = tmp.path().join("f.pdf");
    std::fs::write(&local, b"hello").unwrap();

    let be = RcloneBackend::with_binary("fake:/", &script);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let r = be.upload_with_progress(Path::new(&local), "f.pdf", Some(stored), tx).await;
    drop(tmp);
    r.map(|_| ())
}

#[tokio::test]
async fn delta_stored_md5_passes_precondition_when_stat_prefers_sha1() {
    let r = upload_with_stored_etag("8cdbe53eaf2dcf885824a5c1563e2a4a").await;
    assert!(r.is_ok(), "same content, different hash family must not conflict: {r:?}");
}

#[tokio::test]
async fn uppercase_hash_passes_precondition() {
    let r = upload_with_stored_etag("847BFC9AE8A6AD75E03BBDA5B0200194D2BE6049").await;
    assert!(r.is_ok(), "{r:?}");
}

#[tokio::test]
async fn changed_remote_content_is_still_a_conflict() {
    let r = upload_with_stored_etag("00000000000000000000000000000000").await;
    assert!(matches!(r, Err(SyncError::Conflict { .. })), "{r:?}");
}

/// A stored item ID can't prove the remote is unchanged when the remote
/// has real content hashes — route it through the resolver.
#[tokio::test]
async fn stored_item_id_is_a_conflict_when_remote_has_hashes() {
    let r = upload_with_stored_etag("10TlMHh1Slezu3D1K0NaDI6HIJKVJLWhn").await;
    assert!(matches!(r, Err(SyncError::Conflict { .. })), "{r:?}");
}

/// `list` (used to populate a directory on first readdir) ran lsjson
/// without `--hash`, so every row it created stored the item ID as its
/// ETag — which can never prove the remote unchanged at upload time.
#[tokio::test]
async fn list_requests_content_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("fake-rclone");
    std::fs::write(&script, r#"#!/usr/bin/env bash
if [[ " $* " == *" --hash "* ]]; then
  printf '[{"Path":"a","Name":"a","Size":1,"ModTime":"2024-01-01T00:00:00Z","IsDir":false,"Hashes":{"md5":"8cdbe53eaf2dcf885824a5c1563e2a4a"},"ID":"ID-a"}]'
else
  printf '[{"Path":"a","Name":"a","Size":1,"ModTime":"2024-01-01T00:00:00Z","IsDir":false,"ID":"ID-a"}]'
fi
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let be = RcloneBackend::with_binary("fake:/", &script);
    let entries = be.list("dir").await.unwrap();
    drop(tmp);
    assert_eq!(entries[0].etag.as_deref(), Some("8cdbe53eaf2dcf885824a5c1563e2a4a"));
}
