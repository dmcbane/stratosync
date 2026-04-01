#!/usr/bin/env bash
# setup_repo.sh — Create the GitHub repo and push the initial commit.
# Run this once on your local machine after cloning.
#
# Requires: git, and either:
#   - gh (GitHub CLI):  https://cli.github.com
#   - GITHUB_TOKEN env var with repo creation scope
set -euo pipefail

REPO_NAME="stratosync"
REPO_DESC="On-demand Linux cloud sync daemon — FUSE3 + rclone + SQLite"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "=== stratosync repo bootstrap ==="

# ── Prerequisites ─────────────────────────────────────────────────────────────
if ! command -v git &>/dev/null; then echo "ERROR: git not found"; exit 1; fi
if ! command -v rustc &>/dev/null; then
  echo "ERROR: rustc not found. Install from https://rustup.rs"
  exit 1
fi

RUST_VER=$(rustc --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)
echo "Rust: $RUST_VER"
echo "Repo dir: $REPO_DIR"

# ── Determine auth mode ────────────────────────────────────────────────────────
if command -v gh &>/dev/null && gh auth status &>/dev/null 2>&1; then
  AUTH="gh"
  GITHUB_USER=$(gh api user --jq .login)
elif [[ -n "${GITHUB_TOKEN:-}" ]]; then
  AUTH="token"
  GITHUB_USER=$(curl -sf -H "Authorization: token $GITHUB_TOKEN" \
    https://api.github.com/user | python3 -c "import json,sys; print(json.load(sys.stdin)['login'])")
else
  echo "ERROR: Authenticate with 'gh auth login' or set GITHUB_TOKEN"
  exit 1
fi
echo "GitHub user: $GITHUB_USER ($AUTH)"

REPO_URL="https://github.com/${GITHUB_USER}/${REPO_NAME}"

# ── Create repo if needed ──────────────────────────────────────────────────────
if [[ "$AUTH" == "gh" ]]; then
  if ! gh repo view "${GITHUB_USER}/${REPO_NAME}" &>/dev/null 2>&1; then
    gh repo create "$REPO_NAME" --public --description "$REPO_DESC" --disable-wiki
    echo "Created: $REPO_URL"
  else
    echo "Repo already exists: $REPO_URL"
  fi
else
  STATUS=$(curl -sf -o /dev/null -w "%{http_code}" \
    -H "Authorization: token $GITHUB_TOKEN" \
    "https://api.github.com/repos/${GITHUB_USER}/${REPO_NAME}" || true)
  if [[ "$STATUS" != "200" ]]; then
    curl -sf -H "Authorization: token $GITHUB_TOKEN" \
      -H "Content-Type: application/json" -X POST \
      https://api.github.com/user/repos \
      -d "{\"name\":\"${REPO_NAME}\",\"description\":\"${REPO_DESC}\",\"private\":false,\"auto_init\":false}" \
      > /dev/null
    echo "Created: $REPO_URL"
  else
    echo "Repo already exists: $REPO_URL"
  fi
fi

# ── Set up issue labels ────────────────────────────────────────────────────────
if [[ "$AUTH" == "gh" ]]; then
  echo "Setting up labels..."
  create_label() {
    gh label create "$1" --color "$2" --description "$3" \
      --repo "${GITHUB_USER}/${REPO_NAME}" 2>/dev/null || true
  }
  create_label "phase:1-read-only"   "0075ca" "Phase 1: read-only FUSE"
  create_label "phase:2-sync"        "e4e669" "Phase 2: bidirectional sync"
  create_label "phase:3-delta"       "d93f0b" "Phase 3: delta APIs"
  create_label "phase:4-perf"        "f9d0c4" "Phase 4: performance"
  create_label "component:fuse"      "bfd4f2" "FUSE layer"
  create_label "component:sync"      "bfd4f2" "Sync engine"
  create_label "component:backend"   "bfd4f2" "rclone backend"
  create_label "component:cache"     "bfd4f2" "Cache manager"
  create_label "component:state"     "bfd4f2" "SQLite state DB"
  create_label "component:cli"       "bfd4f2" "CLI"
fi

# ── Initialise git ────────────────────────────────────────────────────────────
cd "$REPO_DIR"
if [[ ! -d .git ]]; then git init; git checkout -b main; fi

if git remote get-url origin &>/dev/null 2>&1; then
  git remote set-url origin "https://github.com/${GITHUB_USER}/${REPO_NAME}.git"
else
  git remote add origin "https://github.com/${GITHUB_USER}/${REPO_NAME}.git"
fi

# ── Quick build verification ───────────────────────────────────────────────────
echo ""
echo "Running cargo check..."
cargo check --workspace 2>&1 | tail -3

echo "Running cargo test..."
cargo test --workspace --quiet 2>&1 | grep "test result"

# ── Commit and push ────────────────────────────────────────────────────────────
git add -A
git commit -m "feat: initial implementation — Phase 1+2 complete

Phase 1 (Read-only VFS):
- FUSE3 filesystem: lookup, getattr, readdir, open, read, release
- On-demand hydration: files fetched from remote on first open()
- Blocking-open pattern: FUSE thread parks on oneshot channel during download
- SQLite WAL state DB with full migration system

Phase 2 (Bidirectional sync):
- Write ops: write, create, mkdir, unlink, rmdir, rename, fsync
- UploadQueue: per-inode debounced uploads with exponential backoff
- ConflictResolver: FNV-hash conflict naming, never loses data
- CacheManager: LRU eviction with configurable quota and marks
- inotify watcher: cache dir → UploadQueue bridge (notify 6.1.1)
- RemotePoller: polling with lsjson diff for remote change detection

Infrastructure:
- RcloneBackend: rclone subprocess adapter for 70+ cloud providers
- MockBackend: in-memory mock for testing (no credentials needed)
- Config: serde-only in core; file I/O isolated to CLI/daemon
- 29 tests: 4 unit + 25 integration (all green)
- systemd user service unit
- install.sh: build, install, enable service

Reference designs studied:
  Syncthing BEP conflict model, Nextcloud Desktop journal DB schema,
  google-drive-ocamlfuse inode stability, rclone VFS cache

Compatibility note: version pins in Cargo.toml for Rust 1.75.
Remove pins on Rust 1.85+ (current stable)." 2>&1

echo ""
echo "Pushing to $REPO_URL ..."
git push -u origin main

echo ""
echo "✓ Done! Repository: $REPO_URL"
echo ""
echo "Next steps:"
echo "  rclone config                    # set up a cloud remote"
echo "  bash install.sh                  # build + install + enable systemd"
echo "  stratosync config test           # verify connectivity"
echo "  systemctl --user start stratosyncd"
