# Vitals Monitor

A real-time web application for **body temperature and blood pressure** sensor data,
built for the INCO course (Innovation and Complexity Management). It has two tiers:

- **Backend** (Rust, following *Zero to Production in Rust*): Actix Web, Tokio, SQLx +
  PostgreSQL with migrations, and `tracing` with Bunyan JSON output. It emits **FHIR
  R4B-compliant JSON** (Observation, Patient, Device, Bundle, OperationOutcome, AuditEvent,
  CapabilityStatement). It also streams live readings over a WebSocket as compact
  **28-byte binary frames**.
- **Frontend** (D3.js v7):
  - live charts with hover tooltips
  - time-window selector, pause, series toggles and a table view
  - an alert log
  - click-to-inspect, which fetches the underlying FHIR Observation over REST

```
 sensors (simulated) ──► ingest pipeline ──► validate (FHIR schema + profile + ranges)
                               │                    │
                               │                    ├─► PostgreSQL (AES-256-GCM encrypted rows)
                               │                    │     └─ retry + circuit breaker + buffer
                               │                    └─► alert engine (thresholds + z-score)
                               ▼
                         stream hub ──WebSocket (binary 28 B / FHIR JSON)──► D3 dashboard
                                              REST /fhir/* (FHIR JSON) ──────► D3 dashboard
```

## Quick start (local)

Requires Rust (https://rustup.rs) and PostgreSQL 17 (`brew install postgresql@17` or
`apt install postgresql`).

```bash
make setup
```

```bash
make dev
```

Then open http://localhost:8000.

`make setup` does four things:

1. Checks that the toolchain is present.
2. Installs the git hooks.
3. Generates secrets and a TLS certificate with `vitalsctl`.
4. Starts a project-local PostgreSQL on port 5433, with its data in `data/pg`.

Migrations run automatically when the server starts.

**Dev test accounts** (only seeded when `APP_ENVIRONMENT` is `dev` or `test`; prod refuses
to start without `users_json`):

| user | password | role | can |
|---|---|---|---|
| `clinician` | `clinician-dev-pass` | clinician | read all patients, stream, write |
| `patient1` | `patient-dev-pass` | patient | only own data (P001) |
| `gateway` | `device-dev-pass` | device | POST observations only |
| `admin` | `admin-dev-pass` | admin | + audit log, key rotation, chaos switch |

## Production-like stack (Docker)

```bash
make up          # postgres + backend (prod config, docker secrets) + nginx (TLS, LB, rate limits)
make up-obs      # + Loki / Alloy / Grafana central logging at http://localhost:3000
```

Then open https://localhost (self-signed certificate). The passwords for the stack are in
`deploy/secrets/initial_passwords.txt`.

## Tests

```bash
make test        # 80 tests: unit, integration (real app + real Postgres), property-based
make cov         # coverage (cargo-llvm-cov): ~85.6 % lines locally; CI gate 80 %
make lint audit  # rustfmt + clippy -D warnings, RustSec advisory scan
```

- **Integration tests** follow the book's approach. Each test starts the real app on a
  random port with its **own freshly migrated database**, so tests run in parallel and never
  share state.
- **Golden-file regression:** `tests/golden/*.json` were produced by the earlier Python
  prototype. The Rust backend reproduces them exactly, so any drift in the emitted FHIR fails
  the build.
- **Property tests (proptest)** cover:
  - binary frame round trips
  - validator fuzzing (it never panics)
  - the simulator always emitting valid FHIR
  - alert flags agreeing with the FHIR interpretation codes
- **CI** runs these jobs:
  - fmt and clippy
  - Linux tests with a Postgres service, coverage, JUnit report and step summary
  - a Linux/macOS/Windows matrix for the DB-free tests
  - `cargo audit`
  - Docker build plus a compose smoke test

## API overview

| Method | Path | Notes |
|---|---|---|
| POST | `/auth/token` | form login → 15 min JWT (rate-limited, audited) |
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

**Fault-tolerance demo:** sign in as `admin` and `POST /admin/chaos/storage?fail=true`.
- The dashboard keeps streaming.
- Writes are buffered.
- `/health/ready` reports `degraded` and the breaker opens.

With `fail=false`, the buffer drains into Postgres and nothing is lost.

## Rubric mapping

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and threat model.
Levels are a self-assessment.

| Criterion | Implemented | Level |
|---|---|---|
| **1. Dev environment** | git, `.githooks` (fmt + clippy + secret guard on commit, tests on push), CI/CD, `rust-cache` dependency caching, `make setup` with `check-env` validation, CI matrix on Linux/macOS/Windows | Advanced → Excellent |
| **2. Testing** | 80 tests: core and edge cases, failure paths, API + DB + FHIR; mocked time (`tokio::time::pause`); proptest property/fuzz; golden-file regression; coverage gate + JUnit + CI summary; nextest retries flag flaky tests | Excellent |
| **3. Configuration** | `config/{base,dev,test,prod}.yaml` layered with `APP_*` env and `*_FILE` secrets, `SecretString` everywhere, startup validation refusing insecure prod config, `vitalsctl rotate-keys` | Advanced |
| **4. Logging** | `tracing` spans with request id + user on every line, Bunyan JSON, levels, PHI policy with a test, FHIR AuditEvent trail, Loki + Grafana dashboard (alerts, failed logins, statuses, latency), example alert rule | Advanced (+ PHI-safe) |
| **5. Deployment** | cargo-chef multi-stage image, non-root, read-only, healthcheck without curl, Postgres service with healthcheck, nginx TLS edge with load balancing and passive health checks, resource limits, CI image build + stack smoke test | Advanced |
| **6. Validation & security** | 3-layer FHIR validation (typed R4B schema + unknown-element check, vital-signs profile, plausibility), parameterised SQL (`QueryBuilder` binds) + DB `CHECK` constraint, XSS/markup rejection, CSP and security headers, body limit, rate limit, `cargo audit` with documented ignores, STRIDE threat model | Advanced → Excellent |
| **7. Error handling** | one `ApiError` → FHIR OperationOutcome with safe messages; details only in spans; extractor errors mapped too; supervised tasks restart after errors **and panics** | Advanced |
| **8. Auth & encryption** | JWT (kid, iss, exp, jti) with secret rotation, RBAC with 4 roles + patient scoping, bcrypt off the async executor, TLS 1.2/1.3 + HSTS, AES-256-GCM at rest with a key ring and re-encryption endpoint, audit log | Advanced |
| **9. Fault tolerance** | retry with backoff + jitter, circuit breaker, bounded fallback buffer with drain, per-client back-pressure (broadcast lag), client auto-reconnect, self-healing supervisor, nginx upstream retry | Advanced |
| **10. FHIR** | vital-signs profiles (bodytemp, bp), LOINC + UCUM, interpretation codes, searchset Bundles, CapabilityStatement, `$validate`, AuditEvent | Advanced |

**Known gaps (and what would close them):**

- **No auto-scaling or high availability.** The simulator and stream hub are in-process,
  so the backend runs as one replica. Redis or NATS pub/sub plus Kubernetes HPA would fix that.
- **No MFA or zero-trust mTLS.**
- **No sync to an external FHIR server.**
- **The Docker stack and CI pipeline have not been run yet.** Docker wasn't running on the
  authoring machine, and the repository hasn't been pushed. CI builds and smoke-tests the
  image once it is pushed to GitHub.
