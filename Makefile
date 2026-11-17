# Convenience targets (macOS/Linux). Windows: use `python scripts/setup.py` + commands in README.
PY  := .venv/bin/python
BIN := .venv/bin

.PHONY: setup dev test cov lint fmt security up up-obs down rotate-keys clean

setup:            ## venv, pinned deps, git hooks, secrets, TLS cert
	python3 scripts/setup.py

dev:              ## run backend + frontend with hot reload on http://localhost:8000
	cd backend && APP_ENV=dev ../$(BIN)/uvicorn app.main:app --reload --port 8000

test:             ## all tests
	cd backend && ../$(PY) -m pytest -q

cov:              ## tests + HTML coverage report (backend/htmlcov/index.html)
	cd backend && ../$(PY) -m pytest --cov --cov-report=html --cov-report=term

lint:
	$(BIN)/ruff check backend && $(BIN)/ruff format --check backend

fmt:
	$(BIN)/ruff check --fix backend && $(BIN)/ruff format backend

security:         ## dependency CVE scan + static analysis
	$(BIN)/pip-audit -r backend/requirements.txt && $(BIN)/bandit -q -ll -r backend/app -c backend/pyproject.toml

up:               ## production-like stack (https://localhost)
	$(PY) scripts/setup.py --secrets && docker compose up --build -d

up-obs:           ## + Loki/Alloy/Grafana (http://localhost:3000)
	$(PY) scripts/setup.py --secrets && docker compose --profile observability up --build -d

down:
	docker compose --profile observability down

rotate-keys:
	$(PY) scripts/rotate_keys.py

clean:
	rm -rf data backend/htmlcov backend/.coverage .pytest_cache .ruff_cache
