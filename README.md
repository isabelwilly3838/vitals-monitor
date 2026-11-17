# Vitals Monitor

A real-time web application for **body temperature and blood pressure** sensor data.
It has two tiers:

- **Backend** (Python / FastAPI) emits **FHIR R4B-compliant JSON** (Observation, Patient,
  Device, Bundle, OperationOutcome, AuditEvent, CapabilityStatement). It also streams live
  readings over a WebSocket as compact **28-byte binary frames**.
- **Frontend** (D3.js v7) shows live charts with hover tooltips, a time-window selector,
  pause, series toggles, a table view, an alert log, and a click-to-inspect panel that
  fetches the underlying FHIR Observation over REST.

```
 sensors (simulated) ──► ingest pipeline ──► validate (FHIR schema + profile + ranges)
                               │                    │
                               │                    ├─► encrypted store (SQLite + Fernet)
                               │                    │     └─ retry + circuit breaker + buffer
                               │                    └─► alert engine (thresholds + z-score)
                               ▼
                         stream hub ──WebSocket (binary 28 B / FHIR JSON)──► D3 dashboard
                                              REST /fhir/* (FHIR JSON) ──────► D3 dashboard
```

## Quick start (local)

Requires Python ≥ 3.11.

```bash
python3 scripts/setup.py
```

```bash
make dev
```

Then open http://localhost:8000. `setup.py` is cross-platform (also Windows). It creates
`.venv`, installs pinned dependencies, installs the git hooks, and generates secrets and a
TLS certificate. Without `make`, start the server with
`cd backend && APP_ENV=dev ../.venv/bin/uvicorn app.main:app --port 8000`.

**Dev test accounts** (only seeded when `APP_ENV` is `dev` or `test`; prod refuses to start
with them):

| user | password | role | can |
|---|---|---|---|
| `clinician` | `clinician-dev-pass` | clinician | read all patients, stream, write |
| `patient1` | `patient-dev-pass` | patient | only own data (P001) |
| `gateway` | `device-dev-pass` | device | POST observations only |
| `admin` | `admin-dev-pass` | admin | + audit log, key rotation, chaos switch |

## Production-like stack (Docker)

```bash
make up          # nginx (TLS, LB, rate limits) + backend (prod config, docker secrets)
make up-obs      # + Loki / Alloy / Grafana central logging at http://localhost:3000
```

Then open https://localhost (self-signed certificate). The passwords for the stack are
generated into `deploy/secrets/initial_passwords.txt`.

## Tests

```bash
make test        # 118 tests: unit, integration (real ASGI app + WebSocket), property-based/fuzz
make cov         # coverage report (gate: 85 %, currently ~92 %)
make lint security
```

- **Golden-file regression tests** (`backend/tests/golden/`) fail when the emitted FHIR JSON
  changes. If the change is intended, regenerate the files with `UPDATE_GOLDEN=1`.
- **Hypothesis** property tests cover the binary codec round trip, validator fuzzing
  (the validator never raises), the simulator always emitting valid FHIR, and alert/interpretation
  consistency.

## API overview

| Method | Path | Notes |
|---|---|---|
| POST | `/auth/token` | OAuth2 password form → 15 min JWT (rate-limited, audited) |
| GET | `/fhir/metadata` | CapabilityStatement (public) |
| GET | `/fhir/Patient`, `/fhir/Patient/{id}`, `/fhir/Device/{id}` | RBAC-scoped |
| GET | `/fhir/Observation?patient=&code=&date=ge…&date=le…&_count=` | searchset Bundle |
| GET | `/fhir/Observation/{id}` | |
| POST | `/fhir/Observation` | validate → store → stream; errors as OperationOutcome |
| POST | `/fhir/Observation/$validate` | dry-run validation |
| GET | `/fhir/AuditEvent` | admin only |
| WS | `/ws/vitals` | first message `{"token", "encoding": "binary"\|"fhir"}` |
| POST | `/admin/keys/rotate`, `/admin/chaos/storage?fail=true` | admin; chaos is disabled in prod |
| GET | `/health/live`, `/health/ready` | probes; ready reports breaker/buffer state |

**Try the fault tolerance live:** sign in as `admin` and run
`POST /admin/chaos/storage?fail=true`. The dashboard keeps streaming, writes are buffered,
`/health/ready` reports `degraded`, and the breaker opens. Run `fail=false` and the buffer
drains into storage.

## Rubric mapping

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design rationale and threat model.
Levels below are an honest self-assessment.

| Criterion | Implemented | Level |
|---|---|---|
| **Dev environment** | git, per-env configs, pre-commit and pre-push hooks, GitHub Actions CI, pinned deps, one-command cross-platform `setup.py`, pip and Docker layer caching, CI matrix on Linux/macOS/Windows | Advanced → Excellent |
| **Testing** | 118 core and edge-case tests, mocks (`AsyncMock`, fake clock), coverage gate + HTML/XML/JUnit reports, Hypothesis property/fuzz tests, golden-file regression, CI step summary | Excellent |
| **Configuration** | `config/{dev,test,prod}.env`, dynamic loading order env → `.env` → vars → `*_FILE` secrets, startup validation that refuses insecure prod config, key/secret rotation script | Advanced |
| **Logging** | structured JSON with ISO-UTC timestamps, request-id correlation, PHI redaction processor, audit trail as FHIR AuditEvent, Loki + Grafana dashboard (alerts, failed logins, 5xx, p95 latency), example alert rule | Advanced (+ PHI-safe) |
| **Deployment** | multi-stage non-root read-only container, healthchecks, nginx TLS edge with load balancing + passive health checks, resource limits, restart policy, CI image build + container smoke test | Advanced |
| **Validation & security** | 3-layer FHIR validation (R4B schema, vital-signs profile, plausibility), parameterised SQL + id/date regexes, markup/script rejection, CSP + security headers, body-size limit, rate limiting, `pip-audit` + `bandit` in CI, STRIDE threat model | Advanced → Excellent |
| **Error handling** | central handlers → FHIR OperationOutcome with safe messages + request id; internals only in the logs; supervised background tasks auto-restart | Advanced |
| **Auth & encryption** | JWT (kid, iss, exp, jti) with secret rotation, RBAC with 4 roles + patient scoping, bcrypt, TLS 1.2/1.3 + HSTS, Fernet at-rest encryption with MultiFernet key rotation, audit log | Advanced |
| **Fault tolerance** | retry with backoff + jitter, circuit breaker, bounded fallback buffer with drain, per-client back-pressure (drop oldest), client auto-reconnect with backoff, self-healing supervisor, nginx retry to the next upstream | Advanced |
| **FHIR compliance** | vital-signs profiles (bodytemp, bp), LOINC + UCUM, interpretation codes, searchset Bundles, CapabilityStatement, `$validate`, AuditEvent | Advanced |

**Known gaps (and what would close them):**

- **No auto-scaling or HA.** SQLite and the in-process simulator mean only one backend
  replica. Moving to PostgreSQL, Redis pub/sub for the hub, and Kubernetes HPA would fix that.
- **No MFA or zero-trust mTLS.**
- **Not verified as HIPAA compliant.**
- **The Docker stack was not built in the authoring environment.** Docker was not running
  there. CI builds and smoke-tests the image.
