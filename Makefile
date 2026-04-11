CARGO := cargo

BOLD  := \033[1m
CYAN  := \033[36m
GREEN := \033[32m
RESET := \033[0m

.DEFAULT_GOAL := help

# Enable Metal automatically on macOS — without it Candle silently falls
# back to CPU and embedding throughput drops ~6x on Apple Silicon.
ifeq ($(shell uname -s),Darwin)
CARGO_RUN_FEATURES := --features metal
endif

# ── Build ────────────────────────────────────────────────────────────────────

.PHONY: build release

build: ## Debug build
	$(CARGO) build $(CARGO_RUN_FEATURES)

release: ## Release build (LTO + strip)
	$(CARGO) build --release $(CARGO_RUN_FEATURES)

# ── Quality ──────────────────────────────────────────────────────────────────

.PHONY: check test lint fmt

check: ## Full quality gate — format, lint, test
	@printf '\n$(BOLD)[1/3] Checking format$(RESET)\n'
	$(CARGO) fmt -- --check
	@printf '\n$(BOLD)[2/3] Running clippy$(RESET)\n'
	$(CARGO) clippy --all-targets -- -D warnings
	@printf '\n$(BOLD)[3/3] Running tests$(RESET)\n'
	$(CARGO) test
	@printf '\n$(GREEN)  ✓ All checks passed$(RESET)\n\n'

test: ## Run tests
	$(CARGO) test

lint: ## Run clippy
	$(CARGO) clippy --all-targets -- -D warnings

fmt: ## Format code
	$(CARGO) fmt

# ── Documentation ────────────────────────────────────────────────────────────

.PHONY: doc

doc: ## Generate API documentation
	$(CARGO) doc --no-deps

# ── Install ──────────────────────────────────────────────────────────────────

.PHONY: install uninstall

install: ## Install binary to ~/.cargo/bin
	$(CARGO) install --path .

uninstall: ## Remove installed binary
	$(CARGO) uninstall recall

# ── Run ──────────────────────────────────────────────────────────────────────

.PHONY: run index sync search

run: ## Launch TUI
	$(CARGO) run $(CARGO_RUN_FEATURES)

index: ## Full index
	$(CARGO) run $(CARGO_RUN_FEATURES) -- index

sync: ## Incremental sync
	$(CARGO) run $(CARGO_RUN_FEATURES) -- sync

search: ## Search sessions (Q="query")
	@test -n "$(Q)" || { printf 'Usage: make search Q="query"\n'; exit 1; }
	$(CARGO) run $(CARGO_RUN_FEATURES) -- search "$(Q)"

# ── Maintenance ──────────────────────────────────────────────────────────────

.PHONY: clean

clean: ## Remove build artifacts
	$(CARGO) clean

# ── Help ─────────────────────────────────────────────────────────────────────

.PHONY: help

help: ## Show available targets
	@awk 'BEGIN {FS = ":.*## "; printf "\n$(BOLD)Recall$(RESET) — local-first AI session search\n"} \
		/^# ── / {n = $$0; gsub(/(^# ── | ─+$$)/, "", n); printf "\n$(BOLD)%s$(RESET)\n", n} \
		/^[a-zA-Z_-]+:.*## / {printf "  $(CYAN)make %-12s$(RESET) %s\n", $$1, $$2} \
		END {printf "\n"}' $(MAKEFILE_LIST)
