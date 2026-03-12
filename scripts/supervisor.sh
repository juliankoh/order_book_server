#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../config.env"

LOG_DIR="$HOME/hl-monitor/logs"
LOG_FILE="$LOG_DIR/supervisor.log"
STATE_FILE="/tmp/hl-supervisor-state"

mkdir -p "$LOG_DIR"

# ─── Logging ──────────────────────────────────────────────
log() {
    local level="$1"; shift
    local msg
    msg="$(date -u +%FT%TZ) [$level]  $*"
    echo "$msg" >> "$LOG_FILE"
    echo "$msg"
}

# ─── State management ────────────────────────────────────
init_state() {
    if [ ! -f "$STATE_FILE" ]; then
        cat > "$STATE_FILE" <<EOF
last_synced_height=0
was_synced=false
last_gossip_update=0
restart_count_node=0
restart_count_ob=0
ob_rapid_restart_count=0
last_ob_restart_time=0
EOF
    fi
}

read_state() {
    source "$STATE_FILE"
}

write_state() {
    cat > "$STATE_FILE" <<EOF
last_synced_height=${last_synced_height:-0}
was_synced=${was_synced:-false}
last_gossip_update=${last_gossip_update:-0}
restart_count_node=${restart_count_node:-0}
restart_count_ob=${restart_count_ob:-0}
ob_rapid_restart_count=${ob_rapid_restart_count:-0}
last_ob_restart_time=${last_ob_restart_time:-0}
EOF
}

# ─── Helpers ──────────────────────────────────────────────
get_local_block_height() {
    local result
    result=$(curl -sf -m 5 -X POST http://localhost:3001/evm \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null) || { echo "0"; return; }
    local hex
    hex=$(echo "$result" | jq -r '.result // "0x0"')
    printf "%d" "$hex" 2>/dev/null || echo "0"
}

get_remote_block_height() {
    local result
    result=$(curl -sf -m 10 -X POST https://api.hyperliquid.xyz/evm \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null) || { echo "0"; return; }
    local hex
    hex=$(echo "$result" | jq -r '.result // "0x0"')
    printf "%d" "$hex" 2>/dev/null || echo "0"
}

get_node_memory_pct() {
    # Get memory % for the hl-node child process (the heavy one), not just hl-visor
    local pid
    pid=$(pgrep -f "hl-node.*run-non-validator" 2>/dev/null | head -1) || {
        # Fallback to visor
        pid=$(pgrep -f "hl-visor run-non-validator" 2>/dev/null | head -1) || { echo "0"; return; }
    }
    ps -o pmem= -p "$pid" 2>/dev/null | tr -d ' ' || echo "0"
}

kill_node_processes() {
    log "INFO" "Killing node processes..."
    # Kill visor first (it will try to kill its child)
    pkill -f "hl-visor run-non-validator" 2>/dev/null || true
    sleep 2
    # Kill any remaining hl-node processes (but NOT anything else with hl-node in name)
    pkill -f "hl-node.*run-non-validator" 2>/dev/null || true

    # Wait for gossip ports to be free
    local tries=0
    while [ $tries -lt 30 ]; do
        if ! ss -tlnp 2>/dev/null | grep -qE ':400[1-6]\b'; then
            break
        fi
        sleep 1
        tries=$((tries + 1))
    done
    if [ $tries -ge 30 ]; then
        log "WARN" "Ports 4001-4006 still occupied after 30s, force killing..."
        for port in 4001 4002 4003 4004 4005 4006; do
            fuser -k "$port/tcp" 2>/dev/null || true
        done
        sleep 2
    fi
    tmux kill-session -t "$TMUX_NODE" 2>/dev/null || true
}

kill_ob_processes() {
    log "INFO" "Killing OB server processes..."
    pkill -f "websocket_server.*--port" 2>/dev/null || true
    tmux kill-session -t "$TMUX_OB" 2>/dev/null || true
    sleep 1
}

start_node() {
    log "INFO" "Starting HL node..."
    kill_node_processes
    sleep "$NODE_RESTART_DELAY"

    # Check for process name collisions before starting
    local collisions
    collisions=$(pgrep -a -f "hl-node" 2>/dev/null | grep -v "hl-visor" | grep -v "grep" | grep -v "hl-monitor" | grep -v "supervisor" || true)
    if [ -n "$collisions" ]; then
        log "WARN" "Process name collision detected! These will cause visor panic:"
        log "WARN" "$collisions"
        log "WARN" "Kill or rename these before starting node."
        return 1
    fi

    tmux new-session -d -s "$TMUX_NODE" -c "$HOME" \
        "exec $VISOR_BIN run-non-validator $NODE_FLAGS 2>&1"
    tmux pipe-pane -t "$TMUX_NODE" -o "cat >> $LOG_DIR/node.log"
    restart_count_node=$((restart_count_node + 1))
    write_state
    log "INFO" "node=STARTED restart_count=$restart_count_node"
}

start_ob() {
    log "INFO" "Starting OB server..."
    kill_ob_processes
    sleep "$OB_RESTART_DELAY"
    # Run with restart loop built in
    tmux new-session -d -s "$TMUX_OB" -c "$HOME" \
        "while true; do RUST_LOG=info $OB_SERVER_BIN --address $OB_SERVER_ADDRESS --port $OB_SERVER_PORT 2>&1; echo 'OB server exited, restarting in ${OB_RESTART_DELAY}s...'; sleep $OB_RESTART_DELAY; done"
    tmux pipe-pane -t "$TMUX_OB" -o "cat >> $LOG_DIR/ob-server.log"

    local now
    now=$(date +%s)
    restart_count_ob=$((restart_count_ob + 1))

    # Track rapid restarts for crash loop detection
    local elapsed=$(( now - ${last_ob_restart_time:-0} ))
    if [ $elapsed -lt 60 ]; then
        ob_rapid_restart_count=$((ob_rapid_restart_count + 1))
    else
        ob_rapid_restart_count=0
    fi
    last_ob_restart_time=$now
    write_state
    log "INFO" "ob=STARTED restart_count=$restart_count_ob"
}

prune_current_hour_data() {
    log "WARN" "Pruning current hour data dirs to recover from OB crash loop..."
    local current_date current_hour
    current_date=$(date -u +%Y%m%d)
    current_hour=$(date -u +%-H)
    for dir in node_raw_book_diffs_by_block node_order_statuses_by_block node_fills_by_block; do
        local target="$NODE_DATA/data/$dir/hourly/$current_date/$current_hour"
        if [ -d "$target" ]; then
            rm -rf "$target"
            log "INFO" "Pruned $target"
        fi
    done
}

check_visor_updating() {
    # Check if visor is updating its child binary (back off if so)
    if tmux capture-pane -t "$TMUX_NODE" -p 2>/dev/null | tail -5 | grep -q "downloading new hl-node binary"; then
        return 0
    fi
    if tmux capture-pane -t "$TMUX_NODE" -p 2>/dev/null | tail -5 | grep -q "restarting child"; then
        return 0
    fi
    return 1
}

update_gossip_peers() {
    log "INFO" "Updating gossip peer config..."
    local tmp_config="${GOSSIP_CONFIG}.tmp"
    if curl -sf -X POST --header "Content-Type: application/json" \
        --data '{ "type": "gossipRootIps" }' https://api.hyperliquid.xyz/info \
        | python3 -c "
import json, sys
ips = json.load(sys.stdin)
config = {'root_node_ips': [{'Ip': ip} for ip in ips], 'try_new_peers': True, 'chain': '${CHAIN}'}
print(json.dumps(config, indent=2))
" > "$tmp_config" 2>/dev/null; then
        if ! diff -q "$GOSSIP_CONFIG" "$tmp_config" &>/dev/null; then
            mv "$tmp_config" "$GOSSIP_CONFIG"
            local count
            count=$(jq '.root_node_ips | length' "$GOSSIP_CONFIG" 2>/dev/null || echo "?")
            log "INFO" "Gossip config updated ($count peers, will apply on next node restart)"
        else
            rm -f "$tmp_config"
            log "INFO" "Gossip config unchanged"
        fi
    else
        rm -f "$tmp_config"
        log "WARN" "Failed to fetch gossip config"
    fi
    last_gossip_update=$(date +%s)
    write_state
}

# ─── Rotate supervisor log ────────────────────────────────
rotate_log() {
    local max_size=52428800  # 50MB
    if [ -f "$LOG_FILE" ]; then
        local size
        size=$(stat -c%s "$LOG_FILE" 2>/dev/null || echo "0")
        if [ "$size" -gt "$max_size" ]; then
            mv "$LOG_FILE" "${LOG_FILE}.$(date -u +%Y%m%dT%H%M%SZ)"
            log "INFO" "Supervisor log rotated"
        fi
    fi
}

# ─── Main loop ────────────────────────────────────────────
init_state
read_state

log "INFO" "========================================="
log "INFO" "Supervisor starting"
log "INFO" "  Health check interval: ${HEALTH_CHECK_INTERVAL}s"
log "INFO" "  Max block lag: ${MAX_BLOCK_LAG}"
log "INFO" "  Max memory: ${MAX_MEMORY_PCT}%"
log "INFO" "  Max disk: ${MAX_DISK_PCT}%"
log "INFO" "========================================="

iteration=0

while true; do
    read_state
    iteration=$((iteration + 1))

    node_status="OK"
    ob_status="OK"
    block_height=0
    block_lag=0
    mem_pct="0"
    disk_pct="0"

    # ─── CHECK NODE ───────────────────────────────────────
    node_pid=$(pgrep -f "hl-visor run-non-validator" 2>/dev/null | head -1 || true)

    if [ -z "$node_pid" ]; then
        node_status="EXITED"
        log "WARN" "node=EXITED action=restarting"
        start_node || true
    else
        # Check if visor is doing a binary update — don't interfere
        if check_visor_updating; then
            log "INFO" "Visor updating child binary, backing off..."
            sleep "$HEALTH_CHECK_INTERVAL"
            continue
        fi

        # Check block progress (sample twice, 5s apart)
        height1=$(get_local_block_height)
        sleep 5
        height2=$(get_local_block_height)

        if [ "$height1" = "$height2" ] && [ "$height2" != "0" ] && [ "$was_synced" = "true" ]; then
            node_status="STUCK"
            log "WARN" "node=STUCK block=$height2 (no change in 5s) action=restarting"
            start_node || true
        else
            block_height=$height2

            # Check lag against chain tip
            remote_height=$(get_remote_block_height)
            if [ "$remote_height" -gt 0 ] && [ "$block_height" -gt 0 ]; then
                block_lag=$((remote_height - block_height))
                if [ $block_lag -lt 0 ]; then block_lag=0; fi

                if [ $block_lag -lt "$MAX_BLOCK_LAG" ]; then
                    if [ "$was_synced" = "false" ]; then
                        log "INFO" "Node synced for the first time! block=$block_height lag=$block_lag"
                        was_synced=true
                    fi
                    last_synced_height=$block_height
                    write_state
                elif [ "$was_synced" = "true" ]; then
                    log "WARN" "node=LAGGING block=$block_height remote=$remote_height lag=$block_lag"
                else
                    # Still catching up from initial sync, don't alarm
                    if [ $((iteration % 6)) -eq 0 ]; then
                        # Log catchup progress every ~60s instead of every check
                        log "INFO" "node=CATCHING_UP block=$block_height remote=$remote_height lag=$block_lag"
                    fi
                fi
            fi

            # Check memory of hl-node child process
            mem_pct=$(get_node_memory_pct)
            mem_int=${mem_pct%.*}
            mem_int=${mem_int:-0}
            if [ "$mem_int" -gt "$MAX_MEMORY_PCT" ] 2>/dev/null; then
                node_status="OOM"
                log "WARN" "node=OOM mem=${mem_pct}% threshold=${MAX_MEMORY_PCT}% action=restarting"
                start_node || true
            fi
        fi
    fi

    # ─── CHECK OB SERVER ─────────────────────────────────
    ob_pid=$(pgrep -f "websocket_server.*--port" 2>/dev/null | head -1 || true)

    if [ -z "$ob_pid" ]; then
        ob_status="EXITED"
        # Only start OB if node is synced
        if [ "$was_synced" = "true" ]; then
            # The tmux restart loop should handle this, but check if the session died
            if ! tmux has-session -t "$TMUX_OB" 2>/dev/null; then
                log "WARN" "ob=SESSION_DEAD action=restarting"

                # Check for crash loop — prune data if 3+ rapid restarts
                if [ "${ob_rapid_restart_count:-0}" -ge 3 ]; then
                    prune_current_hour_data
                    ob_rapid_restart_count=0
                    write_state
                fi

                start_ob
            fi
        else
            ob_status="WAITING"
        fi
    else
        # Check WebSocket port is listening
        if ! ss -tlnp 2>/dev/null | grep -q ":${OB_SERVER_PORT}\b"; then
            ob_status="PORT_CLOSED"
            log "WARN" "ob=PORT_CLOSED (process running but port not listening)"
        fi
    fi

    # ─── CHECK DISK ───────────────────────────────────────
    if mountpoint -q "$NVME_MOUNT" 2>/dev/null; then
        disk_pct=$(df "$NVME_MOUNT" | tail -1 | awk '{print $5}' | tr -d '%')
    else
        disk_pct=$(df "$NODE_DATA" 2>/dev/null | tail -1 | awk '{print $5}' | tr -d '%')
    fi

    if [ "${disk_pct:-0}" -gt "$MAX_DISK_PCT" ] 2>/dev/null; then
        log "WARN" "disk=${disk_pct}% threshold=${MAX_DISK_PCT}% action=pruning"
        bash "$SCRIPT_DIR/prune.sh"
    fi

    # ─── UPDATE GOSSIP PEERS ──────────────────────────────
    now=$(date +%s)
    if [ $((now - ${last_gossip_update:-0})) -ge "$GOSSIP_UPDATE_INTERVAL" ]; then
        update_gossip_peers
    fi

    # ─── ROTATE LOG ───────────────────────────────────────
    if [ $((iteration % 60)) -eq 0 ]; then
        rotate_log
    fi

    # ─── LOG STATUS ───────────────────────────────────────
    log "INFO" "node=$node_status block=$block_height lag=$block_lag mem=${mem_pct}% ob=$ob_status disk=${disk_pct}%"

    sleep "$HEALTH_CHECK_INTERVAL"
done
