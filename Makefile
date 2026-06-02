.PHONY: up down logs test-go test-rust bench lint migrate

up:
	docker compose -f deploy/docker-compose.yml up --build

down:
	docker compose -f deploy/docker-compose.yml down

logs:
	docker compose -f deploy/docker-compose.yml logs -f

test-go:
	cd server && go test -race -count=1 ./...

test-rust:
	cargo test --all --all-features

bench:
	cargo bench --workspace

lint:
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features -- -D warnings
	cd server && golangci-lint run ./...

migrate:
	@echo "Run migrations against PostgreSQL"
