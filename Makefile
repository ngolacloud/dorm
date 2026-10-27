# Thin wrappers over cargo, so the common tasks are one word. `make help` lists
# them. `make check` is exactly what CI runs on the fast lane.
.DEFAULT_GOAL := help

# A throwaway PostgreSQL DSN for the live targets. Override on the command line:
#   make test-live DSN='host=127.0.0.1 user=me dbname=throwaway'
DSN ?= host=/var/run/postgresql user=$(USER) dbname=dorm_dev

.PHONY: help
help: ## List the targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

.PHONY: check
check: ## Format check, lint and test — what CI runs
	cargo fmt --all --check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo test --all-targets

.PHONY: fmt
fmt: ## Format the code
	cargo fmt --all

.PHONY: test
test: ## Run the offline test suite (no database)
	cargo test --all-targets

.PHONY: build
build: ## Build the whole workspace in release
	cargo build --release --workspace

.PHONY: doc
doc: ## Build and open the API docs
	cargo doc --no-deps --workspace --document-private-items --open

.PHONY: shell
shell: ## Run the dynamic REPL (reads DELONIX_ORM_DSN for live queries)
	cargo run -p dorm-shell

.PHONY: test-live
test-live: ## Run the live suites against $(DSN) — needs a throwaway PostgreSQL
	DELONIX_ORM_DSN='$(DSN)' cargo test -p dorm-models --test migrate_live -- --nocapture
	DELONIX_ORM_TEST_DSN='$(DSN)' cargo test -p dorm-models --test typed_e2e -- --ignored --nocapture
