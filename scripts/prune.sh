#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../config.env"

log() { echo "[prune] $(date -u +%FT%TZ) $*"; }

DATA_DIR="$NODE_DATA/data"

# ─── Config ──────────────────────────────────────────────
# How many minutes of data to keep (default: 3 hours)
KEEP_MINUTES=${PRUNE_KEEP_MINUTES:-180}

# Directories pruned by age (keep last KEEP_MINUTES)
PRUNE_BY_AGE=(
    "node_raw_book_diffs_by_block"
    "node_order_statuses_by_block"
    "node_fills_by_block"
    "node_raw_book_diffs"
    "node_order_statuses"
    "node_fills"
    "node_trades"
    "replica_cmds"
    "node_logs"
    "visor_child_stderr"
    "node_slow_block_times"
    "node_fast_block_times"
    "node_twap_statuses_by_block"
    "latency_summaries"
    "latency_buckets"
)

# Number of periodic_abci_states snapshots to keep
KEEP_SNAPSHOTS=2

log "Starting prune (keep last ${KEEP_MINUTES}m of data, ${KEEP_SNAPSHOTS} snapshots)..."

total_freed=0

# ─── Prune directories by age ───────────────────────────
for dir in "${PRUNE_BY_AGE[@]}"; do
    target="$DATA_DIR/$dir"
    if [ ! -d "$target" ]; then
        continue
    fi

    # Calculate size before
    size_before=$(du -sb "$target" 2>/dev/null | awk '{print $1}' || echo "0")

    # Delete files older than KEEP_MINUTES
    find "$target" -type f -mmin +"$KEEP_MINUTES" -delete 2>/dev/null || true

    # Remove empty directories
    find "$target" -type d -empty -delete 2>/dev/null || true

    # Calculate size after
    size_after=$(du -sb "$target" 2>/dev/null | awk '{print $1}' || echo "0")

    freed=$((size_before - size_after))
    if [ $freed -gt 0 ]; then
        freed_mb=$((freed / 1048576))
        log "$dir: freed ${freed_mb}MB"
        total_freed=$((total_freed + freed))
    fi
done

# ─── Prune periodic_abci_states (keep N newest) ─────────
SNAP_DIR="$DATA_DIR/periodic_abci_states"
if [ -d "$SNAP_DIR" ]; then
    size_before=$(du -sb "$SNAP_DIR" 2>/dev/null | awk '{print $1}' || echo "0")

    # List snapshot dirs sorted newest first, delete all but KEEP_SNAPSHOTS
    old_snaps=$(ls -dt "$SNAP_DIR"/*/ 2>/dev/null | tail -n +"$((KEEP_SNAPSHOTS + 1))" || true)
    if [ -n "$old_snaps" ]; then
        echo "$old_snaps" | xargs rm -rf
    fi

    size_after=$(du -sb "$SNAP_DIR" 2>/dev/null | awk '{print $1}' || echo "0")
    freed=$((size_before - size_after))
    if [ $freed -gt 0 ]; then
        freed_mb=$((freed / 1048576))
        log "periodic_abci_states: freed ${freed_mb}MB (kept ${KEEP_SNAPSHOTS} snapshots)"
        total_freed=$((total_freed + freed))
    fi
fi

# ─── Clean tmp dirs ──────────────────────────────────────
for tmp_dir in "$NODE_DATA/tmp/fu_write_string_to_file_tmp" "$NODE_DATA/tmp/shell_rs_out"; do
    if [ -d "$tmp_dir" ]; then
        size_before=$(du -sb "$tmp_dir" 2>/dev/null | awk '{print $1}' || echo "0")
        find "$tmp_dir" -type f -mmin +60 -delete 2>/dev/null || true
        size_after=$(du -sb "$tmp_dir" 2>/dev/null | awk '{print $1}' || echo "0")
        freed=$((size_before - size_after))
        if [ $freed -gt 0 ]; then
            freed_mb=$((freed / 1048576))
            log "$(basename "$tmp_dir"): freed ${freed_mb}MB"
            total_freed=$((total_freed + freed))
        fi
    fi
done

# ─── Rotate service logs if over 100MB ──────────────────
LOG_DIR="$HOME/hl-monitor/logs"
for logfile in "$LOG_DIR/node.log" "$LOG_DIR/ob-server.log"; do
    if [ -f "$logfile" ]; then
        size=$(stat -c%s "$logfile" 2>/dev/null || echo "0")
        if [ "$size" -gt 104857600 ]; then
            tail -1000 "$logfile" > "${logfile}.tmp"
            mv "${logfile}.tmp" "$logfile"
            log "Rotated $logfile (was $((size / 1048576))MB)"
        fi
    fi
done

# ─── Prune old supervisor log rotations ──────────────────
if [ -d "$LOG_DIR" ]; then
    find "$LOG_DIR" -type f -name "*.log.*" -mtime +7 -delete 2>/dev/null || true
fi

# ─── Summary ─────────────────────────────────────────────
total_freed_mb=$((total_freed / 1048576))
log "Prune complete. Total freed: ${total_freed_mb}MB"

# Show current disk usage
if mountpoint -q "$NVME_MOUNT" 2>/dev/null; then
    log "Disk: $(df -h "$NVME_MOUNT" | tail -1 | awk '{print $3 " / " $2 " (" $5 ")"}')"
fi
