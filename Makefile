CARGO := cargo

BOLD  := \033[1m
CYAN  := \033[36m
GREEN := \033[32m
RESET := \033[0m

.DEFAULT_GOAL := help

# ── Build ────────────────────────────────────────────────────────────────────

.PHONY: build release

build: ## Debug build
	$(CARGO) build

release: ## Release build (LTO + strip)
	$(CARGO) build --release

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

.PHONY: run sync search

run: ## Launch TUI
	$(CARGO) run

sync: ## Incremental sync (use FORCE=1 to reprocess all)
	$(CARGO) run -- sync $(if $(FORCE),--force,)

search: ## Search sessions (Q="query")
	@test -n "$(Q)" || { printf 'Usage: make search Q="query"\n'; exit 1; }
	$(CARGO) run -- search "$(Q)"

# ── Release ──────────────────────────────────────────────────────────────────

.PHONY: release-patch release-minor release-major

release-patch: ## Bump patch + commit + tag + push (append EXECUTE=1 to apply)
	cargo release patch $(if $(EXECUTE),--execute,)

release-minor: ## Bump minor + commit + tag + push (append EXECUTE=1 to apply)
	cargo release minor $(if $(EXECUTE),--execute,)

release-major: ## Bump major + commit + tag + push (append EXECUTE=1 to apply)
	cargo release major $(if $(EXECUTE),--execute,)

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
