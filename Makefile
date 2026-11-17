# Developer workflow (macOS / Linux; on Windows use WSL or `docker compose`).
SHELL   := /bin/bash
# rustup from Homebrew is keg-only: make cargo findable without editing ~/.zshrc
export PATH := $(HOME)/.cargo/bin:/opt/homebrew/opt/rustup/bin:/usr/local/opt/rustup/bin:$(PATH)
PG_BIN  ?= $(shell pg_config --bindir 2>/dev/null || echo /opt/homebrew/opt/postgresql@17/bin)
PGDATA  := data/pg
PGPORT  := 5433
PSQL    := $(PG_BIN)/psql -h localhost -p $(PGPORT) -U postgres
CARGO   := cd backend && cargo

.PHONY: setup check-env db db-stop dev test cov lint fmt audit up up-obs down rotate-keys clean-test-dbs

setup: check-env           ## hooks, secrets, TLS cert, local database
	git config core.hooksPath .githooks
	$(CARGO) run -q --bin vitalsctl -- gen-secrets
	$(MAKE) db
	@echo; echo "Ready. Run: make dev   -> http://localhost:8000"

check-env:                 ## automatic environment validation
	@command -v cargo >/dev/null || { echo "Rust missing: https://rustup.rs"; exit 1; }
	@test -x "$(PG_BIN)/pg_ctl" || { echo "PostgreSQL missing (brew install postgresql@17 / apt install postgresql)"; exit 1; }
	@echo "rust: $$(rustc --version) | postgres: $$($(PG_BIN)/postgres --version)"

db:                        ## start the project-local PostgreSQL on :5433 (data in ./data/pg)
	@test -d $(PGDATA) || LC_ALL=en_US.UTF-8 $(PG_BIN)/initdb -D $(PGDATA) -U postgres --auth=trust -E UTF8 >/dev/null
	@$(PG_BIN)/pg_ctl -D $(PGDATA) status >/dev/null || LC_ALL=en_US.UTF-8 $(PG_BIN)/pg_ctl -D $(PGDATA) -o "-p $(PGPORT) -k /tmp" -l data/pg.log start >/dev/null
	@sleep 1
	@$(PSQL) -tAc "SELECT 1 FROM pg_database WHERE datname='vitals'" | grep -q 1 || $(PSQL) -qc "CREATE DATABASE vitals"
	@echo "postgres running on localhost:$(PGPORT)"

db-stop:
	$(PG_BIN)/pg_ctl -D $(PGDATA) stop

dev: db                    ## backend + frontend on http://localhost:8000 (migrations run on start)
	$(CARGO) run --bin vitals_monitor

test: db                   ## unit + integration (fresh DB per test) + property tests
	$(CARGO) test

cov: db                    ## coverage report (needs: cargo install cargo-llvm-cov)
	$(CARGO) llvm-cov --html --output-dir ../target-cov && echo "open target-cov/html/index.html"

lint:
	$(CARGO) fmt --check && cargo clippy --all-targets -- -D warnings

fmt:
	$(CARGO) fmt

audit:                     ## dependency vulnerability scan (needs: cargo install cargo-audit)
	$(CARGO) audit

clean-test-dbs:            ## drop the per-test databases created by the integration tests
	@for db in $$($(PSQL) -tAc "SELECT datname FROM pg_database WHERE datname LIKE 'test_%'"); do \
	  $(PSQL) -qc "DROP DATABASE \"$$db\""; done; echo "test databases dropped"

up:                        ## production-like stack: https://localhost
	$(CARGO) run -q --bin vitalsctl -- gen-secrets
	docker compose up --build -d

up-obs:                    ## + Loki / Alloy / Grafana on http://localhost:3000
	$(CARGO) run -q --bin vitalsctl -- gen-secrets
	docker compose --profile observability up --build -d

down:
	docker compose --profile observability down

rotate-keys:
	$(CARGO) run -q --bin vitalsctl -- rotate-keys
