include config.mk
export

LOG_DIR = $(HOME)/hl-monitor/logs
NODE_LOG = $(LOG_DIR)/node.log
OB_LOG = $(LOG_DIR)/ob-server.log

.PHONY: install start start-streaming start-node start-node-streaming start-ob start-ob-streaming start-supervisor \
        stop stop-node stop-ob stop-supervisor \
        restart restart-node restart-ob build-ob status health watch logs-node logs-ob logs-supervisor \
        prune update-peers update-visor disk nuke help check-names

help: ## Show available commands
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'

# ─── First-time setup ────────────────────────────────────
install: ## Full first-time setup: mount NVMe, download binaries, build OB server
	bash scripts/install.sh

# ─── Service control ─────────────────────────────────────
start: update-peers start-node start-ob start-supervisor ## Start everything and open log view
	@echo ""
	@echo "All services started. Opening log view..."
	@echo "(Ctrl+B, D to detach without stopping services)"
	@sleep 1
	@$(MAKE) watch

start-node: ## Start just the HL node in tmux
	@mkdir -p $(LOG_DIR)
	@if tmux has-session -t $(TMUX_NODE) 2>/dev/null; then \
		echo "Node session '$(TMUX_NODE)' already running"; \
	else \
		echo "Starting HL node in tmux session '$(TMUX_NODE)'..."; \
		tmux new-session -d -s $(TMUX_NODE) -c $(HOME) \
			'exec $(VISOR_BIN) run-non-validator $(NODE_FLAGS) 2>&1'; \
		tmux pipe-pane -t $(TMUX_NODE) -o 'cat >> $(NODE_LOG)'; \
		echo "Node started. Logs: $(NODE_LOG)"; \
	fi

start-ob: ## Start just the OB server in tmux (with restart loop)
	@mkdir -p $(LOG_DIR)
	@if tmux has-session -t $(TMUX_OB) 2>/dev/null; then \
		echo "OB server session '$(TMUX_OB)' already running"; \
	else \
		echo "Starting OB server in tmux session '$(TMUX_OB)'..."; \
		tmux new-session -d -s $(TMUX_OB) -c $(HOME) \
			'while true; do RUST_LOG=info $(OB_SERVER_BIN) --address $(OB_SERVER_ADDRESS) --port $(OB_SERVER_PORT) 2>&1; echo "OB server exited, restarting in $(OB_RESTART_DELAY)s..."; sleep $(OB_RESTART_DELAY); done'; \
		tmux pipe-pane -t $(TMUX_OB) -o 'cat >> $(OB_LOG)'; \
		echo "OB server started (with restart loop). Logs: $(OB_LOG)"; \
	fi

start-supervisor: ## Start the supervisor daemon in tmux
	@if tmux has-session -t $(TMUX_SUPERVISOR) 2>/dev/null; then \
		echo "Supervisor session '$(TMUX_SUPERVISOR)' already running"; \
	else \
		echo "Starting supervisor in tmux session '$(TMUX_SUPERVISOR)'..."; \
		tmux new-session -d -s $(TMUX_SUPERVISOR) 'bash scripts/supervisor.sh'; \
		echo "Supervisor started."; \
	fi

start-streaming: update-peers start-node-streaming start-ob-streaming start-supervisor ## Start everything in streaming mode
	@echo ""
	@echo "All services started (streaming mode). Opening log view..."
	@echo "(Ctrl+B, D to detach without stopping services)"
	@sleep 1
	@$(MAKE) watch

start-node-streaming: ## Start the HL node in streaming mode
	@mkdir -p $(LOG_DIR)
	@if tmux has-session -t $(TMUX_NODE) 2>/dev/null; then \
		echo "Node session '$(TMUX_NODE)' already running"; \
	else \
		echo "Starting HL node in tmux session '$(TMUX_NODE)' (streaming mode)..."; \
		tmux new-session -d -s $(TMUX_NODE) -c $(HOME) \
			'exec $(VISOR_BIN) run-non-validator $(NODE_FLAGS_STREAMING) 2>&1'; \
		tmux pipe-pane -t $(TMUX_NODE) -o 'cat >> $(NODE_LOG)'; \
		echo "Node started (streaming). Logs: $(NODE_LOG)"; \
	fi

start-ob-streaming: ## Start the OB server in streaming mode
	@mkdir -p $(LOG_DIR)
	@if tmux has-session -t $(TMUX_OB) 2>/dev/null; then \
		echo "OB server session '$(TMUX_OB)' already running"; \
	else \
		echo "Starting OB server in tmux session '$(TMUX_OB)' (streaming mode)..."; \
		tmux new-session -d -s $(TMUX_OB) -c $(HOME) \
			'while true; do RUST_LOG=info $(OB_SERVER_BIN) --address $(OB_SERVER_ADDRESS) --port $(OB_SERVER_PORT) --streaming 2>&1; echo "OB server exited, restarting in $(OB_RESTART_DELAY)s..."; sleep $(OB_RESTART_DELAY); done'; \
		tmux pipe-pane -t $(TMUX_OB) -o 'cat >> $(OB_LOG)'; \
		echo "OB server started (streaming, with restart loop). Logs: $(OB_LOG)"; \
	fi

stop: stop-supervisor stop-ob stop-node ## Stop everything gracefully

stop-node: ## Stop just the HL node
	@echo "Stopping HL node..."
	@-pkill -f "hl-visor run-non-validator" 2>/dev/null; true
	@sleep 1
	@-pkill -f "hl-node.*run-non-validator" 2>/dev/null; true
	@-tmux kill-session -t $(TMUX_NODE) 2>/dev/null; true
	@echo "Waiting for ports to free..."
	@for i in $$(seq 1 15); do \
		if ! ss -tlnp 2>/dev/null | grep -qE ':400[1-6]\b'; then break; fi; \
		sleep 1; \
	done
	@echo "Node stopped."

stop-ob: ## Stop just the OB server
	@echo "Stopping OB server..."
	@-pkill -f "websocket_server.*--port" 2>/dev/null; true
	@-tmux kill-session -t $(TMUX_OB) 2>/dev/null; true
	@echo "OB server stopped."

stop-supervisor: ## Stop the supervisor
	@echo "Stopping supervisor..."
	@-tmux kill-session -t $(TMUX_SUPERVISOR) 2>/dev/null; true
	@echo "Supervisor stopped."

restart: stop start ## Restart everything

restart-node: stop-node start-node ## Restart just the node

build-ob: ## Build OB server release binary
	@echo "Building OB server..."
	@cargo build --release --manifest-path $(OB_SERVER_SRC)Cargo.toml
	@echo "OB server built."

restart-ob: stop-ob build-ob ## Rebuild and restart just the OB server
	@echo "Clearing OB server log..."
	@> $(OB_LOG)
	@$(MAKE) start-ob

# ─── Monitoring ───────────────────────────────────────────
status: ## Show status of all components
	@bash scripts/health.sh

health: status ## Alias for status

watch: ## Split-pane view: node (top) + OB server (bottom)
	@mkdir -p $(LOG_DIR)
	@touch $(NODE_LOG) $(OB_LOG)
	@tmux kill-session -t hl-watch 2>/dev/null || true
	@tmux new-session -d -s hl-watch "tail -f $(NODE_LOG)"
	@tmux split-window -t hl-watch -v "tail -f $(OB_LOG)"
	@tmux select-pane -t hl-watch:0.0
	@tmux attach-session -t hl-watch

logs-node: ## Tail node logs
	@tail -f $(NODE_LOG) 2>/dev/null || echo "No node logs yet"

logs-ob: ## Tail OB server logs
	@tail -f $(OB_LOG) 2>/dev/null || echo "No OB server logs yet"

logs-supervisor: ## Tail supervisor logs
	@tail -f $(LOG_DIR)/supervisor.log 2>/dev/null || echo "No supervisor logs yet"

# ─── Maintenance ──────────────────────────────────────────
prune: ## Manually trigger log pruning
	bash scripts/prune.sh

update-peers: ## Fetch latest gossip peer list from API + seed peers
	@echo "Fetching latest gossip peers..."
	@python3 scripts/update-peers.py $(CHAIN) \
		> $(GOSSIP_CONFIG).tmp \
		&& mv $(GOSSIP_CONFIG).tmp $(GOSSIP_CONFIG) \
		&& echo "Gossip config updated: $(GOSSIP_CONFIG) ($$(jq '.root_node_ips | length' $(GOSSIP_CONFIG)) peers)" \
		|| (rm -f $(GOSSIP_CONFIG).tmp && echo "Failed to update gossip config")

update-visor: ## Download latest hl-visor binary
	@echo "Downloading latest hl-visor..."
	@curl -sSL https://binaries.hyperliquid.xyz/$(CHAIN)/hl-visor -o $(VISOR_BIN).tmp \
		&& chmod +x $(VISOR_BIN).tmp \
		&& mv $(VISOR_BIN).tmp $(VISOR_BIN) \
		&& echo "hl-visor updated." \
		|| echo "Failed to download hl-visor"

disk: ## Show disk usage breakdown
	@echo "=== Disk Usage ==="
	@df -h $(NVME_MOUNT) 2>/dev/null || df -h $(NODE_DATA)
	@echo ""
	@echo "=== Data Directory Breakdown ==="
	@du -sh $(NODE_DATA)/data/*/ 2>/dev/null | sort -rh | head -20

check-names: ## Check for process name collisions (binaries containing 'hl-node')
	@echo "Checking for process name collisions..."
	@procs=$$(pgrep -a -f "hl-node" 2>/dev/null | grep -v "hl-visor" | grep -v "grep" | grep -v "hl-monitor" || true); \
	if [ -n "$$procs" ]; then \
		echo "WARNING: Found processes with 'hl-node' in name that could collide with visor:"; \
		echo "$$procs"; \
		echo "Rename these binaries to avoid visor panic on restart."; \
	else \
		echo "No collisions found."; \
	fi

# ─── Cleanup ──────────────────────────────────────────────
nuke: ## Kill everything, wipe data, start fresh (DANGEROUS)
	@echo "WARNING: This will kill all processes and wipe all node data."
	@echo "Press Ctrl+C to cancel, or Enter to continue..."
	@read _confirm
	$(MAKE) stop
	rm -rf $(NODE_DATA)/data
	rm -f /tmp/hl-supervisor-state
	rm -rf $(LOG_DIR)
	@echo "All data wiped. Run 'make install && make start' to start fresh."
