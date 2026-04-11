/// 3-way merge support via `git merge-file`.
///
/// This module lives in `stratosync-core` so both the daemon (automatic
/// conflict resolution) and the CLI (`conflicts merge` subcommand) can
/// reuse the same logic without cross-crate dependency.
use std::path::Path;

/// Result of attempting a 3-way merge via `git merge-file`.
pub enum MergeOutcome {
    /// All changes merged cleanly — no conflict markers.
    Clean(Vec<u8>),
    /// Merge produced output with conflict markers (`<<<<<<<` / `=======` / `>>>>>>>`).
    ConflictMarkers(Vec<u8>),
    /// Merge could not run (git not found, I/O error, etc.).
    Failed(String),
}

/// Check whether `git merge-file` is available on `$PATH`.
/// Returns true if `git --version` succeeds.
pub fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Attempt a 3-way merge using `git merge-file --stdout`.
///
/// Arguments follow git merge-file convention:
///   `git merge-file --stdout <local> <base> <remote>`
///
/// Exit codes:
///   0 = clean merge (stdout = merged content)
///   1 = conflicts (stdout = merged content with markers)
///   other = error
pub fn try_three_way_merge(
    base_path:   &Path,
    local_path:  &Path,
    remote_path: &Path,
) -> MergeOutcome {
    let output = match std::process::Command::new("git")
        .args([
            "merge-file", "--stdout",
            &local_path.to_string_lossy(),
            &base_path.to_string_lossy(),
            &remote_path.to_string_lossy(),
        ])
        .output()
    {
        Ok(o) => o,
        Err(e) => return MergeOutcome::Failed(format!("git merge-file: {e}")),
    };

    match output.status.code() {
        Some(0) => MergeOutcome::Clean(output.stdout),
        Some(1) => MergeOutcome::ConflictMarkers(output.stdout),
        Some(code) => MergeOutcome::Failed(format!(
            "git merge-file exited {code}: {}",
            String::from_utf8_lossy(&output.stderr)
        )),
        None => MergeOutcome::Failed("git merge-file killed by signal".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn merge_clean_non_overlapping() {
        if !git_available() { return; }

        let dir = tempfile::tempdir().unwrap();
        let base   = dir.path().join("base.txt");
        let local  = dir.path().join("local.txt");
        let remote = dir.path().join("remote.txt");

        std::fs::write(&base,   "line1\nline2\nline3\n").unwrap();
        std::fs::write(&local,  "LOCAL\nline2\nline3\n").unwrap();
        std::fs::write(&remote, "line1\nline2\nREMOTE\n").unwrap();

        match try_three_way_merge(&base, &local, &remote) {
            MergeOutcome::Clean(merged) => {
                assert_eq!(String::from_utf8_lossy(&merged), "LOCAL\nline2\nREMOTE\n");
            }
            MergeOutcome::ConflictMarkers(_) => panic!("expected Clean, got ConflictMarkers"),
            MergeOutcome::Failed(msg) => panic!("expected Clean, got Failed: {msg}"),
        }
    }

    #[test]
    fn merge_conflict_markers_overlapping() {
        if !git_available() { return; }

        let dir = tempfile::tempdir().unwrap();
        let base   = dir.path().join("base.txt");
        let local  = dir.path().join("local.txt");
        let remote = dir.path().join("remote.txt");

        std::fs::write(&base,   "line1\n").unwrap();
        std::fs::write(&local,  "LOCAL\n").unwrap();
        std::fs::write(&remote, "REMOTE\n").unwrap();

        match try_three_way_merge(&base, &local, &remote) {
            MergeOutcome::ConflictMarkers(merged) => {
                let text = String::from_utf8_lossy(&merged);
                assert!(text.contains("<<<<<<<"), "should contain conflict markers");
                assert!(text.contains(">>>>>>>"), "should contain conflict markers");
            }
            MergeOutcome::Clean(_) => panic!("expected ConflictMarkers, got Clean"),
            MergeOutcome::Failed(msg) => panic!("expected ConflictMarkers, got Failed: {msg}"),
        }
    }

    #[test]
    fn merge_failed_bad_paths() {
        if !git_available() { return; }

        let result = try_three_way_merge(
            Path::new("/nonexistent/base"),
            Path::new("/nonexistent/local"),
            Path::new("/nonexistent/remote"),
        );
        assert!(matches!(result, MergeOutcome::Failed(_)));
    }

    #[test]
    fn git_available_returns_bool() {
        // Just verify it doesn't panic — actual availability is environment-dependent.
        let _ = git_available();
    }
}
