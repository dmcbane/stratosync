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
# Shared file-manager action wrapper used by Dolphin / Thunar / PCManFM.
# Goes to BIN_DIR so it's always on the same PATH as the `stratosync` CLI.
install -m 755 "${REPO_DIR}/contrib/file-managers/bin/stratosync-fm-action" \
    "${BIN_DIR}/stratosync-fm-action"
echo "✓ Installed stratosyncd        → ${BIN_DIR}/stratosyncd"
echo "✓ Installed stratosync         → ${BIN_DIR}/stratosync"
echo "✓ Installed stratosync-fm-action → ${BIN_DIR}/stratosync-fm-action"

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

# ── File-manager extensions (Nautilus / Nemo / Caja) ──────────────────────────
#
# Each extension is a thin GObject shell over `stratosync_fm_common.py`.
# We install the helper alongside whichever extension(s) the user's
# desktop has bindings for. Missing python3-{nautilus,nemo,caja}
# packages are not errors — that desktop is simply skipped.
COMMON_SRC="${REPO_DIR}/contrib/file-managers/common/stratosync_fm_common.py"

install_fm_extension() {
    local fm_name="$1"          # "nautilus" | "nemo" | "caja"
    local gi_module="$2"        # "Nautilus" | "Nemo" | "Caja"
    local versions="$3"         # space-separated, e.g. "4.0 3.0"
    local ext_filename="$4"     # "stratosync_nautilus.py" etc.

    local ext_src="${REPO_DIR}/contrib/file-managers/${fm_name}/${ext_filename}"
    local ext_dir="${HOME}/.local/share/${fm_name}-python/extensions"

    [[ -f "$ext_src" && -f "$COMMON_SRC" ]] || return 0

    local found=0
    for v in $versions; do
        if python3 -c "import gi; gi.require_version('${gi_module}','${v}')" \
                2>/dev/null; then
            found=1
            break
        fi
    done
    if [[ $found -eq 0 ]]; then
        echo "  ${fm_name} extension available but python3-${fm_name} not found — skipping"
        return 0
    fi

    mkdir -p "${ext_dir}"
    install -m 644 "$COMMON_SRC" "${ext_dir}/stratosync_fm_common.py"
    install -m 644 "$ext_src"    "${ext_dir}/"
    echo "✓ ${fm_name} extension installed (restart: ${fm_name} -q)"
}

install_fm_extension nautilus Nautilus "4.0 3.0" stratosync_nautilus.py
install_fm_extension nemo     Nemo     "3.0"     stratosync_nemo.py
install_fm_extension caja     Caja     "2.0"     stratosync_caja.py

# ── Declarative-action file managers (Dolphin / Thunar / PCManFM) ─────────────
#
# These don't load Python plugins — they read .desktop / uca.xml files and
# call `Exec=` directly. We always copy the files into their XDG paths;
# they're harmless when the file manager isn't installed.

# Dolphin (KDE) — KIO ServiceMenu
KDE_SERVICEMENU_DIR="${HOME}/.local/share/kio/servicemenus"
DOLPHIN_SRC="${REPO_DIR}/contrib/file-managers/dolphin/stratosync.desktop"
if [[ -f "$DOLPHIN_SRC" ]]; then
    mkdir -p "${KDE_SERVICEMENU_DIR}"
    install -m 755 "$DOLPHIN_SRC" "${KDE_SERVICEMENU_DIR}/stratosync.desktop"
    echo "✓ Dolphin/Konqueror service menu installed (restart Dolphin)"
fi

# PCManFM / PCManFM-Qt — FreeDesktop file-manager Actions spec
PCMANFM_ACTIONS_DIR="${HOME}/.local/share/file-manager/actions"
PCMANFM_SRC_DIR="${REPO_DIR}/contrib/file-managers/pcmanfm"
if compgen -G "${PCMANFM_SRC_DIR}/stratosync-*.desktop" > /dev/null; then
    mkdir -p "${PCMANFM_ACTIONS_DIR}"
    for f in "${PCMANFM_SRC_DIR}"/stratosync-*.desktop; do
        install -m 644 "$f" "${PCMANFM_ACTIONS_DIR}/$(basename "$f")"
    done
    echo "✓ PCManFM/PCManFM-Qt actions installed (restart PCManFM)"
fi

# Thunar (XFCE) — UCA snippet. We do NOT auto-merge into the user's
# uca.xml; that file may already contain custom actions and clobbering
# them would be hostile. Just print a hint.
if command -v thunar &>/dev/null; then
    echo "  Thunar detected — see ${REPO_DIR}/contrib/file-managers/thunar/README.md"
    echo "    for instructions on merging stratosync-uca.xml into ~/.config/Thunar/uca.xml"
fi

# ── Fuse configuration ────────────────────────────────────────────────────────
FUSE_CONF="/etc/fuse.conf"
if [[ -f "$FUSE_CONF" ]] && ! grep -q "^user_allow_other" "$FUSE_CONF"; then
    echo ""
    echo "OPTIONAL: To allow other users (root) to access your mounts, add:"
    echo "  user_allow_other"
    echo "to ${FUSE_CONF} (requires sudo), then set allow_other=true in config."
fi

# ── Shell completions ────────────────────────────────────────────────────────
echo ""
echo "Shell completions (recommended):"
SHELL_NAME="$(basename "${SHELL:-/bin/bash}")"
case "$SHELL_NAME" in
    bash)
        echo "  Add to ~/.bashrc:"
        echo "    source <(COMPLETE=bash stratosync)"
        ;;
    zsh)
        echo "  Add to ~/.zshrc:"
        echo "    source <(COMPLETE=zsh stratosync)"
        ;;
    fish)
        echo "  Add to ~/.config/fish/config.fish:"
        echo "    COMPLETE=fish stratosync | source"
        ;;
    *)
        echo "  Run 'stratosync completions' for setup instructions."
        ;;
esac

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
