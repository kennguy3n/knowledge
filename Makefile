# Knowledge — root Makefile.
# Run `make help` for available targets.

COMPOSE := docker compose -f deploy/docker-compose.yml

.PHONY: help up down logs test-go test-rust bench lint migrate fmt

help: ## Show this help.
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

up: ## Start all services (build images first).
	$(COMPOSE) up --build -d

down: ## Stop and remove all containers.
	$(COMPOSE) down

logs: ## Tail logs for all services.
	$(COMPOSE) logs -f

test-go: ## Run Go server tests.
	cd server && go test -race -count=1 ./...

test-rust: ## Run all Rust workspace tests.
	cargo test --all --all-features

bench: ## Run the Rust criterion benchmark suite.
	cargo bench -p benchmarks

lint: ## Run Rust clippy + fmt check and Go linters.
	cargo clippy --all-targets --all-features -- -D warnings
	cargo fmt --all -- --check
	cd server && golangci-lint run ./...

fmt: ## Auto-format Rust and Go code.
	cargo fmt --all
	cd server && gofmt -w .

migrate: ## Run gateway Postgres migrations (via the gateway binary).
	@echo "Migrations are applied automatically on gateway startup."
	@echo "To run manually, start the gateway with KNOWLEDGE_DATABASE_URL set."
