//! Unix-domain-socket IPC server for the dashboard.
//!
//! Protocol: line-delimited JSON, one request per connection.
//!     → {"op":"status"}\n
//!     ← {"ok":true,"data":{...DaemonStatus...}}\n
//! Any unknown op returns `{"ok":false,"error":"..."}`.
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use stratosync_core::ipc::{IpcRequest, IpcResponse};

use crate::state::DaemonState;

pub async fn serve(socket_path: PathBuf, state: Arc<DaemonState>) -> Result<()> {
    // Remove any stale socket left by a prior crash — bind will fail otherwise.
    let _ = tokio::fs::remove_file(&socket_path).await;

    // Ensure parent exists (e.g. /run/user/$UID already exists on systemd
    // but $XDG_RUNTIME_DIR may point elsewhere).
    if let Some(parent) = socket_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    // Restrict to owner: only this user can query the daemon.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", socket_path.display()))?;

    info!(path = %socket_path.display(), "IPC socket listening");

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("IPC accept failed: {e}");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state).await {
                debug!("IPC connection error: {e}");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, state: Arc<DaemonState>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    let read_fut = reader.read_line(&mut line);
    match timeout(Duration::from_secs(2), read_fut).await {
        Ok(Ok(0)) => return Ok(()), // client closed without sending
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => {
            let _ = send_response(&mut write_half,
                &IpcResponse::err("read timeout")).await;
            return Ok(());
        }
    }

    let req: IpcRequest = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            let _ = send_response(&mut write_half,
                &IpcResponse::err(format!("invalid request: {e}"))).await;
            return Ok(());
        }
    };

    let response = dispatch(&req, &state).await;
    send_response(&mut write_half, &response).await?;
    Ok(())
}

async fn dispatch(req: &IpcRequest, state: &DaemonState) -> IpcResponse {
    match req.op.as_str() {
        "status" => {
            let snap = state.snapshot().await;
            match serde_json::to_value(&snap) {
                Ok(v)  => IpcResponse::ok(v),
                Err(e) => IpcResponse::err(format!("serialize: {e}")),
            }
        }
        other => IpcResponse::err(format!("unknown op: {other}")),
    }
}

async fn send_response(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &IpcResponse,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(resp)?;
    bytes.push(b'\n');
    write_half.write_all(&bytes).await?;
    write_half.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MountHandle;
    use crate::sync::UploadQueue;
    use dashmap::DashMap;
    use std::time::Duration;
    use stratosync_core::{
        backend::mock::MockBackend,
        base_store::BaseStore,
        config::SyncConfig,
        ipc::{DaemonStatus, PollerStatus},
        state::{NewFileEntry, StateDb},
        types::{FileKind, SyncStatus, FUSE_ROOT_INODE},
        Backend,
    };

    async fn build_state() -> (Arc<DaemonState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(StateDb::in_memory().unwrap());
        db.migrate().await.unwrap();
        let mount_id = db.upsert_mount(
            "test", "mock:/", "/mnt/test",
            dir.path().to_str().unwrap(), 5 << 30, 60,
        ).await.unwrap();
        db.insert_root(&NewFileEntry {
            mount_id, parent: 0,
            name: "/".into(), remote_path: "/".into(),
            kind: FileKind::Directory, size: 0,
            mtime: std::time::SystemTime::UNIX_EPOCH, etag: None,
            status: SyncStatus::Remote,
            cache_path: None, cache_size: None,
        }).await.unwrap();

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::default());
        let base_store = Arc::new(BaseStore::new(dir.path().join(".bases")).unwrap());
        let sync_config = Arc::new(SyncConfig::default());
        let upload_queue = Arc::new(UploadQueue::new(
            mount_id, Arc::clone(&db), Arc::clone(&backend),
            Arc::clone(&base_store), Arc::clone(&sync_config),
            Duration::from_secs(60), Duration::from_millis(500), 2,
            None,  // no bandwidth schedule in tests
            0,     // versioning disabled in tests
        ));
        let poller_state = Arc::new(tokio::sync::RwLock::new(PollerStatus {
            mode: "full-listing".into(),
            current_interval_secs: 60,
            ..Default::default()
        }));

        let handle = MountHandle {
            name: "test".into(),
            remote: "mock:/".into(),
            mount_path: "/mnt/test".into(),
            quota_bytes: 5 << 30,
            mount_id,
            db,
            upload_queue,
            poller_state,
            hydration_waiters: Arc::new(DashMap::new()),
        };
        (Arc::new(DaemonState::new(vec![handle])), dir)
    }

    #[tokio::test]
    async fn status_roundtrip_over_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let (state, _td) = build_state().await;

        // Server task
        let sock_for_server = sock.clone();
        let server_handle = tokio::spawn(async move {
            let _ = serve(sock_for_server, state).await;
        });

        // Wait briefly for the listener to be ready
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Client
        let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        stream.write_all(b"{\"op\":\"status\"}\n").await.unwrap();
        stream.shutdown().await.unwrap();

        let mut buf = String::new();
        let (rd, _) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(rd);
        reader.read_line(&mut buf).await.unwrap();

        let resp: IpcResponse = serde_json::from_str(buf.trim()).unwrap();
        assert!(resp.ok, "response not ok: {:?}", resp.error);
        let status: DaemonStatus = serde_json::from_value(resp.data.unwrap()).unwrap();
        assert_eq!(status.mounts.len(), 1);
        assert_eq!(status.mounts[0].name, "test");
        assert_eq!(status.mounts[0].cache.quota_bytes, 5 << 30);

        server_handle.abort();
    }

    #[tokio::test]
    async fn unknown_op_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test2.sock");
        let (state, _td) = build_state().await;
        let sock_for_server = sock.clone();
        let server_handle = tokio::spawn(async move {
            let _ = serve(sock_for_server, state).await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        stream.write_all(b"{\"op\":\"does-not-exist\"}\n").await.unwrap();
        stream.shutdown().await.unwrap();

        let mut buf = String::new();
        let (rd, _) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(rd);
        reader.read_line(&mut buf).await.unwrap();

        let resp: IpcResponse = serde_json::from_str(buf.trim()).unwrap();
        assert!(!resp.ok);
        assert!(resp.error.unwrap().contains("unknown op"));

        server_handle.abort();
    }
}
