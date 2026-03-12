#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../config.env"

log() { echo "[install] $(date -u +%FT%TZ) $*"; }

# ─── 1. System dependencies ──────────────────────────────
log "Installing system dependencies..."
sudo apt-get update -qq
sudo apt-get install -y -qq build-essential pkg-config libssl-dev tmux jq sysstat curl git python3

# ─── 2. Mount NVMe ────────────────────────────────────────
if [ -b "$NVME_DEVICE" ]; then
    log "NVMe device $NVME_DEVICE found."

    # Format if not already xfs
    if ! blkid "$NVME_DEVICE" | grep -q 'TYPE="xfs"'; then
        log "Formatting $NVME_DEVICE as xfs..."
        sudo mkfs.xfs "$NVME_DEVICE"
    else
        log "Already formatted as xfs."
    fi

    # Create mount point and mount
    sudo mkdir -p "$NVME_MOUNT"
    if ! mountpoint -q "$NVME_MOUNT"; then
        sudo mount "$NVME_DEVICE" "$NVME_MOUNT"
        log "Mounted $NVME_DEVICE at $NVME_MOUNT"
    else
        log "Already mounted at $NVME_MOUNT"
    fi

    # Set ownership
    sudo chown "$(whoami):$(whoami)" "$NVME_MOUNT"

    # Create data dir and symlink
    mkdir -p "$NVME_MOUNT/hl"
    if [ -L "$NODE_DATA" ]; then
        log "Symlink $NODE_DATA already exists."
    elif [ -d "$NODE_DATA" ]; then
        log "WARNING: $NODE_DATA exists as a directory. Moving contents..."
        mv "$NODE_DATA"/* "$NVME_MOUNT/hl/" 2>/dev/null || true
        rmdir "$NODE_DATA"
        ln -s "$NVME_MOUNT/hl" "$NODE_DATA"
    else
        rm -f "$NODE_DATA" 2>/dev/null || true
        ln -s "$NVME_MOUNT/hl" "$NODE_DATA"
    fi
    log "Symlink: $NODE_DATA -> $NVME_MOUNT/hl"

    # Add to fstab if not already present (nofail so boot doesn't break if NVMe is gone)
    if ! grep -q "$NVME_DEVICE" /etc/fstab; then
        echo "$NVME_DEVICE $NVME_MOUNT xfs defaults,nofail 0 2" | sudo tee -a /etc/fstab > /dev/null
        log "Added to /etc/fstab with nofail."
    fi
else
    log "WARNING: NVMe device $NVME_DEVICE not found. Using $NODE_DATA directly."
    mkdir -p "$NODE_DATA"
fi

# ─── 3. Download hl-visor ─────────────────────────────────
log "Downloading hl-visor..."
curl -sSL "https://binaries.hyperliquid.xyz/${CHAIN}/hl-visor" -o "$VISOR_BIN"
chmod +x "$VISOR_BIN"
log "hl-visor downloaded to $VISOR_BIN"

# Verify GPG signature
log "Verifying GPG signature..."
curl -sL https://raw.githubusercontent.com/hyperliquid-dex/node/main/pub_key.asc | gpg --import 2>/dev/null || true
curl -sSL "https://binaries.hyperliquid.xyz/${CHAIN}/hl-visor.asc" -o "${VISOR_BIN}.asc"
if gpg --verify "${VISOR_BIN}.asc" "$VISOR_BIN" 2>&1 | grep -q "Good signature"; then
    log "GPG signature verified."
else
    log "WARNING: GPG signature verification failed or unavailable."
fi
rm -f "${VISOR_BIN}.asc"

# ─── 4. Write visor.json (must be in $HOME) ──────────────
log "Writing ~/visor.json..."
cat > "$HOME/visor.json" <<EOF
{"chain": "${CHAIN}"}
EOF
log "visor.json written to $HOME/visor.json"

# ─── 5. Fetch gossip peer config ──────────────────────────
log "Fetching gossip peer config from API..."
if curl -sf -X POST --header "Content-Type: application/json" \
    --data '{ "type": "gossipRootIps" }' https://api.hyperliquid.xyz/info \
    | python3 -c "
import json, sys
ips = json.load(sys.stdin)
config = {'root_node_ips': [{'Ip': ip} for ip in ips], 'try_new_peers': True, 'chain': '${CHAIN}'}
print(json.dumps(config, indent=2))
" > "$GOSSIP_CONFIG"; then
    peer_count=$(jq '.root_node_ips | length' "$GOSSIP_CONFIG" 2>/dev/null || echo "0")
    log "Gossip config written to $GOSSIP_CONFIG ($peer_count peers)"
else
    log "WARNING: Failed to fetch gossip config. Node may have trouble finding peers."
fi

# ─── 6. Install Rust ──────────────────────────────────────
if ! command -v cargo &>/dev/null; then
    log "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
    log "Rust installed."
else
    log "Rust already installed: $(rustc --version)"
fi

# ─── 7. Build order_book_server ───────────────────────────
OB_DIR="$SCRIPT_DIR/.."
log "Building order_book_server (release mode)..."
cd "$OB_DIR" && cargo build --release
log "order_book_server built."

# ─── 8. Create log directory ──────────────────────────────
mkdir -p "$HOME/hl-monitor/logs"

# ─── 9. Check for process name collisions ─────────────────
log "Checking for process name collisions..."
collisions=$(find "$HOME" -maxdepth 4 -type f -executable -name "*hl-node*" 2>/dev/null \
    | grep -v "hl-visor" | grep -v "hl-monitor" | grep -v ".git" || true)
if [ -n "$collisions" ]; then
    log "WARNING: Found executables with 'hl-node' in name:"
    log "$collisions"
    log "The visor will panic if these are running. Rename them!"
fi

# ─── Summary ──────────────────────────────────────────────
log ""
log "========================================="
log "  Installation complete!"
log "========================================="
log "  hl-visor:         $VISOR_BIN"
log "  visor.json:       $HOME/visor.json"
log "  Node data:        $NODE_DATA -> $NVME_MOUNT/hl"
log "  OB server:        $OB_SERVER_BIN"
log "  Gossip config:    $GOSSIP_CONFIG"
log "  Logs:             $HOME/hl-monitor/logs/"
log ""
log "  Next: make start"
log "========================================="
