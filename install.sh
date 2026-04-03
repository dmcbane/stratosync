#!/usr/bin/env bash
# install.sh — Build stratosync and install it on the local machine.
#
# Usage:
#   ./install.sh           # install to ~/.local/bin, enable user service
#   ./install.sh --prefix /usr/local   # install to /usr/local/bin (needs sudo)
#   ./install.sh --no-service          # skip systemd service setup
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREFIX="${HOME}/.local"
INSTALL_SERVICE=true
BUILD_RELEASE=true

# ── Parse args ────────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)    PREFIX="$2"; shift 2 ;;
        --no-service) INSTALL_SERVICE=false; shift ;;
        --debug)     BUILD_RELEASE=false; shift ;;
        -h|--help)
            echo "Usage: $0 [--prefix DIR] [--no-service] [--debug]"
            exit 0 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

BIN_DIR="${PREFIX}/bin"
echo "=== stratosync installer ==="
echo "Install prefix: ${PREFIX}"
echo ""

# ── Prerequisites ─────────────────────────────────────────────────────────────
check_dep() {
    if ! command -v "$1" &>/dev/null; then
        echo "ERROR: $1 not found. $2"
        exit 1
    fi
}
check_dep rustc "Install from https://rustup.rs"
check_dep cargo "Install from https://rustup.rs"
check_dep rclone "Install from https://rclone.org/install/"

RUST_VERSION=$(rustc --version | grep -oE '[0-9]+\.[0-9]+' | head -1)
RUST_MAJOR=$(echo "$RUST_VERSION" | cut -d. -f1)
RUST_MINOR=$(echo "$RUST_VERSION" | cut -d. -f2)
if [[ $RUST_MAJOR -lt 1 ]] || [[ $RUST_MAJOR -eq 1 && $RUST_MINOR -lt 80 ]]; then
    echo "ERROR: Rust 1.80+ required (found ${RUST_VERSION})"
    echo "Run: rustup update stable"
    exit 1
fi
echo "✓ Rust ${RUST_VERSION}"
echo "✓ rclone $(rclone --version 2>/dev/null | head -1)"

# Check for FUSE3 kernel module / libfuse3
if ! pkg-config --exists fuse3 2>/dev/null; then
    echo "WARNING: fuse3 dev headers not found."
    echo "  On Debian/Ubuntu: sudo apt install libfuse3-dev"
    echo "  On Fedora/RHEL:   sudo dnf install fuse3-devel"
    echo "  Continuing anyway (may fail to compile)..."
fi

# ── Build ─────────────────────────────────────────────────────────────────────
echo ""
echo "Building stratosync..."
cd "$REPO_DIR"

BUILD_FLAGS=""
TARGET_DIR="target/debug"
if [[ "$BUILD_RELEASE" == "true" ]]; then
    BUILD_FLAGS="--release"
    TARGET_DIR="target/release"
fi

cargo build $BUILD_FLAGS \
    -p stratosync-daemon \
    -p stratosync-cli \
    2>&1

echo ""
echo "Build complete."

# ── Install binaries ──────────────────────────────────────────────────────────
mkdir -p "${BIN_DIR}"
install -m 755 "${REPO_DIR}/${TARGET_DIR}/stratosyncd"  "${BIN_DIR}/stratosyncd"
install -m 755 "${REPO_DIR}/${TARGET_DIR}/stratosync"   "${BIN_DIR}/stratosync"
echo "✓ Installed stratosyncd → ${BIN_DIR}/stratosyncd"
echo "✓ Installed stratosync  → ${BIN_DIR}/stratosync"

# ── Shell PATH hint ───────────────────────────────────────────────────────────
if [[ ":$PATH:" != *":${BIN_DIR}:"* ]]; then
    echo ""
    echo "NOTE: ${BIN_DIR} is not in your PATH."
    echo "Add this to ~/.bashrc or ~/.zshrc:"
    echo "  export PATH=\"${BIN_DIR}:\$PATH\""
fi

# ── Create default config dir ─────────────────────────────────────────────────
CONFIG_DIR="${HOME}/.config/stratosync"
mkdir -p "${CONFIG_DIR}"

if [[ ! -f "${CONFIG_DIR}/config.toml" ]]; then
    cat > "${CONFIG_DIR}/config.toml" << 'TOML'
# stratosync configuration
# Run `stratosync config --help` for documentation.

[daemon]
log_level = "info"

# Add a mount — run `rclone config` first to set up a remote, then:
# [[mount]]
# name          = "gdrive"
# remote        = "gdrive:/"
# mount_path    = "~/GoogleDrive"
# cache_quota   = "10 GiB"
# poll_interval = "30s"
TOML
    echo "✓ Created default config at ${CONFIG_DIR}/config.toml"
    echo "  Edit it to add your cloud mounts."
else
    echo "✓ Config already exists at ${CONFIG_DIR}/config.toml"
fi

# ── Systemd user service ──────────────────────────────────────────────────────
if [[ "$INSTALL_SERVICE" == "true" ]]; then
    SYSTEMD_DIR="${HOME}/.config/systemd/user"
    mkdir -p "${SYSTEMD_DIR}"

    sed "s|%h|${HOME}|g" \
        "${REPO_DIR}/contrib/systemd/stratosyncd.service" \
        > "${SYSTEMD_DIR}/stratosyncd.service"

    if command -v systemctl &>/dev/null; then
        systemctl --user daemon-reload
        systemctl --user enable stratosyncd.service
        echo "✓ Systemd user service installed and enabled"
        echo ""
        echo "To start now:    systemctl --user start stratosyncd"
        echo "To check status: systemctl --user status stratosyncd"
        echo "To view logs:    journalctl --user -u stratosyncd -f"
    else
        echo "✓ Systemd unit installed at ${SYSTEMD_DIR}/stratosyncd.service"
        echo "  (systemctl not available — enable manually)"
    fi
fi

# ── Fuse configuration ────────────────────────────────────────────────────────
FUSE_CONF="/etc/fuse.conf"
if [[ -f "$FUSE_CONF" ]] && ! grep -q "^user_allow_other" "$FUSE_CONF"; then
    echo ""
    echo "OPTIONAL: To allow other users (root) to access your mounts, add:"
    echo "  user_allow_other"
    echo "to ${FUSE_CONF} (requires sudo), then set allow_other=true in config."
fi

# ── Done ──────────────────────────────────────────────────────────────────────
echo ""
echo "=== Installation complete ==="
echo ""
echo "Quick start:"
echo "  1. Configure a cloud remote:  rclone config"
echo "  2. Edit config:               \$EDITOR ${CONFIG_DIR}/config.toml"
echo "  3. Test connectivity:         stratosync config test"
echo "  4. Start daemon:              systemctl --user start stratosyncd"
echo "  5. Check status:              stratosync status"
