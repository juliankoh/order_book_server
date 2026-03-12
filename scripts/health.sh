#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/../config.env"

STATE_FILE="/tmp/hl-supervisor-state"

# ─── Helpers ──────────────────────────────────────────────
get_local_block_height() {
    curl -sf -m 5 -X POST http://localhost:3001/evm \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null \
        | jq -r '.result // "0x0"' \
        | xargs printf "%d" 2>/dev/null || echo "0"
}

get_remote_block_height() {
    curl -sf -m 10 -X POST https://api.hyperliquid.xyz/evm \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null \
        | jq -r '.result // "0x0"' \
        | xargs printf "%d" 2>/dev/null || echo "0"
}

fmt_uptime() {
    local pid=$1
    if [ -z "$pid" ]; then echo "n/a"; return; fi
    ps -o etime= -p "$pid" 2>/dev/null | tr -d ' ' || echo "n/a"
}

mark() {
    if [ "$1" = "ok" ]; then echo "OK"; else echo "FAIL"; fi
}

# ─── Gather data ─────────────────────────────────────────
timestamp=$(date -u +%FT%TZ)

# Node process
node_pid=$(pgrep -f "hl-visor run-non-validator" 2>/dev/null | head -1 || true)
node_child_pid=$(pgrep -f "hl-node.*run-non-validator" 2>/dev/null | head -1 || true)
if [ -n "$node_pid" ]; then
    node_status="RUNNING (visor=$node_pid"
    if [ -n "$node_child_pid" ]; then
        node_status="$node_status node=$node_child_pid)"
    else
        node_status="$node_status node=starting)"
    fi
    node_ok="ok"
else
    node_status="NOT RUNNING"
    node_ok="fail"
fi

# Block height
local_height=$(get_local_block_height)
remote_height=$(get_remote_block_height)
if [ "$remote_height" -gt 0 ] && [ "$local_height" -gt 0 ]; then
    block_lag=$((remote_height - local_height))
    if [ $block_lag -lt 0 ]; then block_lag=0; fi
    if [ $block_lag -le "$MAX_BLOCK_LAG" ]; then
        block_ok="ok"
    else
        block_ok="fail"
    fi
    block_info="$local_height (tip: $remote_height, lag: $block_lag)"
else
    block_lag=0
    block_ok="fail"
    block_info="unavailable (local=$local_height remote=$remote_height)"
fi

# Memory (check hl-node child, not just visor)
mem_pid="${node_child_pid:-$node_pid}"
if [ -n "$mem_pid" ]; then
    mem_pct=$(ps -o pmem= -p "$mem_pid" 2>/dev/null | tr -d ' ' || echo "0")
    mem_int=${mem_pct%.*}
    mem_int=${mem_int:-0}
    mem_rss_kb=$(ps -o rss= -p "$mem_pid" 2>/dev/null | tr -d ' ' || echo "0")
    mem_rss_gb=$(awk "BEGIN {printf \"%.1f\", $mem_rss_kb / 1048576}")
    total_mem_kb=$(grep MemTotal /proc/meminfo 2>/dev/null | awk '{print $2}' || echo "0")
    total_mem_gb=$(awk "BEGIN {printf \"%.0f\", $total_mem_kb / 1048576}")
    if [ "$mem_int" -le "$MAX_MEMORY_PCT" ] 2>/dev/null; then
        mem_ok="ok"
    else
        mem_ok="fail"
    fi
    mem_info="${mem_pct}% (${mem_rss_gb}GB / ${total_mem_gb}GB)"
else
    mem_ok="fail"
    mem_info="n/a"
fi

# OB Server
ob_pid=$(pgrep -f "websocket_server.*--port" 2>/dev/null | head -1 || true)
if [ -n "$ob_pid" ]; then
    ob_status="RUNNING (pid $ob_pid)"
    ob_ok="ok"
else
    ob_status="NOT RUNNING"
    ob_ok="fail"
fi

# OB block processing
ob_block_info="n/a"
ob_block_ok="fail"
if [ -n "$ob_pid" ]; then
    last_ob_line=$(tmux capture-pane -t "$TMUX_OB" -p 2>/dev/null | grep "OrderDiffs block" | tail -1 || true)
    if [ -n "$last_ob_line" ]; then
        last_block=$(echo "$last_ob_line" | grep -oP 'block: \K[0-9]+' || echo "?")
        ob_block_info="processing (last: $last_block)"
        ob_block_ok="ok"
    else
        ob_block_info="no recent diffs (may be OK if just started)"
        ob_block_ok="ok"
    fi
fi

# WebSocket
ws_ok="fail"
ws_info="n/a"
if [ -n "$ob_pid" ]; then
    if ss -tlnp 2>/dev/null | grep -q ":${OB_SERVER_PORT}\b"; then
        ws_ok="ok"
        ws_info="ws://localhost:${OB_SERVER_PORT} listening"
    else
        ws_info="port ${OB_SERVER_PORT} not listening"
    fi
fi

# Disk
if mountpoint -q "$NVME_MOUNT" 2>/dev/null; then
    disk_line=$(df -h "$NVME_MOUNT" | tail -1)
else
    disk_line=$(df -h "$NODE_DATA" 2>/dev/null | tail -1 || echo "- - - - -")
fi
disk_used=$(echo "$disk_line" | awk '{print $3}')
disk_total=$(echo "$disk_line" | awk '{print $2}')
disk_pct=$(echo "$disk_line" | awk '{print $5}')
disk_pct_num=${disk_pct%%%}
if [ "${disk_pct_num:-0}" -le "$MAX_DISK_PCT" ] 2>/dev/null; then
    disk_ok="ok"
else
    disk_ok="fail"
fi
disk_info="$disk_used / $disk_total ($disk_pct)"

# Peers
peer_count=0
peer_age="n/a"
peer_ok="fail"
if [ -f "$GOSSIP_CONFIG" ]; then
    peer_count=$(jq '.root_node_ips | length' "$GOSSIP_CONFIG" 2>/dev/null || echo "0")
    file_mtime=$(stat -c %Y "$GOSSIP_CONFIG" 2>/dev/null || echo "0")
    now=$(date +%s)
    age_secs=$((now - file_mtime))
    if [ $age_secs -lt 3600 ]; then
        peer_age="$((age_secs / 60))m ago"
    else
        peer_age="$((age_secs / 3600))h ago"
    fi
    if [ "$peer_count" -gt 0 ]; then
        peer_ok="ok"
    fi
fi

# Supervisor
sup_pid=""
if tmux has-session -t "$TMUX_SUPERVISOR" 2>/dev/null; then
    sup_status="RUNNING"
    sup_ok="ok"
else
    sup_status="NOT RUNNING"
    sup_ok="fail"
fi

# Process name collisions
collision_info="none"
collision_ok="ok"
collisions=$(pgrep -a -f "hl-node" 2>/dev/null | grep -v "hl-visor" | grep -v "grep" | grep -v "hl-monitor" | grep -v "run-non-validator" || true)
if [ -n "$collisions" ]; then
    collision_info="WARNING: found"
    collision_ok="fail"
fi

# Uptime
node_uptime=$(fmt_uptime "$node_pid")
ob_uptime=$(fmt_uptime "$ob_pid")

# Restart counts
node_restarts="?"
ob_restarts="?"
if [ -f "$STATE_FILE" ]; then
    source "$STATE_FILE"
    node_restarts="${restart_count_node:-0}"
    ob_restarts="${restart_count_ob:-0}"
fi

# ─── Output ───────────────────────────────────────────────
echo ""
echo "HL Stack Health Check -- $timestamp"
echo "---------------------------------------------"
printf "Node:        %-50s %s\n" "$node_status" "$(mark $node_ok)"
printf "Block:       %-50s %s\n" "$block_info" "$(mark $block_ok)"
printf "Memory:      %-50s %s\n" "$mem_info" "$(mark $mem_ok)"
printf "OB Server:   %-50s %s\n" "$ob_status" "$(mark $ob_ok)"
printf "OB Blocks:   %-50s %s\n" "$ob_block_info" "$(mark $ob_block_ok)"
printf "WebSocket:   %-50s %s\n" "$ws_info" "$(mark $ws_ok)"
printf "Disk:        %-50s %s\n" "$disk_info" "$(mark $disk_ok)"
printf "Peers:       %-50s %s\n" "$peer_count configured, updated $peer_age" "$(mark $peer_ok)"
printf "Supervisor:  %-50s %s\n" "$sup_status" "$(mark $sup_ok)"
printf "Collisions:  %-50s %s\n" "$collision_info" "$(mark $collision_ok)"
printf "Uptime:      node %s, ob-server %s\n" "$node_uptime" "$ob_uptime"
printf "Restarts:    node %s, ob-server %s\n" "$node_restarts" "$ob_restarts"
echo ""
