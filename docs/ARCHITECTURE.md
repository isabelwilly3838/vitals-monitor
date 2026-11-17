# Architecture & design notes

The backend follows *Zero to Production in Rust* (Palmieri), the course book.

| Book concept | Where it lives here |
|---|---|
| `Application::build` + random port | `startup.rs` |
| Layered `Settings` with `base.yaml` / `<env>.yaml` / `APP_*` env | `configuration.rs` |
| `get_subscriber` / `init_subscriber`, Bunyan, `TracingLogger` | `telemetry.rs`, `startup.rs` |
| Fresh database per integration test, `spawn_app()` | `tests/api/helpers.rs` |
| SQLx migrations | `migrations/` |
| "Subscriber validation and rejection" | applied to sensor data in `fhir/validation.rs` |
| `ResponseError` with error categories | `error.rs` |
| `secrecy::SecretString` | passwords, JWT secrets, keys, DB password |
| cargo-chef Docker build | `backend/Dockerfile` |

## Layout

```
backend/
  migrations/              resources + audit_events tables
  src/
    main.rs                tracing init -> configuration -> Application::build -> run
    configuration.rs       layered settings, *_FILE secrets, prod hardening
    telemetry.rs           Bunyan JSON subscriber, spawn_blocking_with_tracing
    startup.rs             wiring, routes, middleware, background tasks, root span
    error.rs               ApiError -> FHIR OperationOutcome
    fhir/codes.rs          LOINC / UCUM / thresholds
    fhir/builders.rs       resource builders + schema check against typed fhir-sdk R4B models
    fhir/validation.rs     3-layer validation + XSS/control-char scan
    security/auth.rs       roles, permissions, bcrypt users, JWT with kid rotation, extractor
    security/crypto.rs     AES-256-GCM key ring (fingerprinted keys, rotation)
    security/middleware.rs request id + security headers, token-bucket rate limiter
    services/store.rs      PgStore (parameterised SQL, encrypted rows) + ResilientStore
    services/resilience.rs retry, CircuitBreaker, supervise()
    services/stream.rs     28-byte frame codec + broadcast hub
    services/anomaly.rs    threshold + z-score alerting
    services/simulator.rs  wearable sensor simulator
    services/state.rs      AppState + ingest pipeline + audit helper
    routes/                auth, fhir, ops (health/admin), ws
    bin/vitalsctl.rs       secrets, cert, key rotation, config check, container healthcheck
  tests/
    api/                   black-box integration tests (real HTTP, real Postgres, real WS)
    fhir.rs                builders, golden files, validator edge cases
    property.rs            proptest properties and fuzzing
frontend/                  D3.js dashboard (unchanged API contract)
config/                    base.yaml, dev.yaml, test.yaml, prod.yaml
deploy/                    nginx, observability (Alloy, Loki, Grafana)
```

## Why a binary stream next to FHIR?

FHIR JSON is the system of record and the interoperability format. Everything stored and
served over REST is a validated FHIR R4B resource. As FHIR JSON, a 1 Hz feed is about 1.6 kB
per tick per patient. The binary frame carries the same reading in **28 bytes, roughly 98 %
less**.

The frame keeps what is needed to get back to FHIR: the patient index and the millisecond
timestamp. When you click a point, the dashboard runs
`GET /fhir/Observation?patient=…&code=…&date=eq<ts>` to fetch the canonical resource.

## Data flow per sample

1. The simulator (or `POST /fhir/Observation`) produces readings.
2. The builders create vital-signs Observations, and every one is validated, including the
   ones we generate.
3. `ResilientStore::save` writes them through the circuit breaker, with retries using
   backoff and jitter.
   - Only index columns are stored in clear text; the FHIR JSON is AES-256-GCM encrypted.
   - If storage fails, the write goes into a bounded buffer and is drained every 2 s by a
     supervised task. The live stream never waits on the database.
4. `AlertEngine` sets the alert flags. It logs only on transitions.
5. `Hub::publish` sends the sample to a `tokio::broadcast` channel.
   - A slow WebSocket client *lags*: it skips its oldest frames instead of blocking the
     producer.

## Threat model (STRIDE, summary)

| Threat | Example | Mitigation |
|---|---|---|
| **S**poofing | stolen or forged token | 15-min HS256 JWT with `iss`/`exp`/`kid`; `alg=none` rejected (tested); bcrypt; login rate limit (app + nginx); same error and timing for unknown user and wrong password |
| **T**ampering | malicious Observation, SQLi | typed FHIR parse + unknown-element check + profile rules, `QueryBuilder` binds, id regex + DB `CHECK`, AES-GCM authentication tag detects tampering at rest |
| **R**epudiation | "I never viewed that record" | FHIR AuditEvent for login, read, search and create (success and failure), encrypted, admin-only |
| **I**nformation disclosure | PHI in logs, token in URL, XSS | `SecretString`/redacted `KeyRing` Debug, spans `skip` values (tested), WS token sent in the first message, in-memory token in the browser, CSP, text-only DOM writes, encryption at rest, TLS + HSTS, patient-scoped RBAC |
| **D**enial of service | large or compressed bodies, slow clients, login floods | 64 kB streaming body limit, actix compression disabled (no decompression bombs), nginx `limit_req`, bounded broadcast, `_count` ≤ 2000 |
| **E**levation of privilege | patient reading another patient's data | permission matrix per role plus a per-patient check on every read, search and stream subscription (tested) |

## Dependency security

`cargo audit` runs in CI. Two advisories are ignored in `backend/.cargo/audit.toml`, each
with a justification: `rsa` and `libsqlite3-sys`. Both appear in `Cargo.lock` only because
`sqlx-macros` lists every database driver; neither is compiled (`cargo tree -i rsa` prints
nothing). Actix's HTTP/2 and compression features are switched off, because nginx handles
them at the edge. That removed the `h2` and decompression advisories altogether.

## Key rotation

- **Encryption:** `vitalsctl rotate-keys` puts a new key first and restarts. Then call
  `POST /admin/keys/rotate` to re-encrypt every row with the primary key, and finish with
  `vitalsctl rotate-keys --prune`.
- **JWT:** the new secret signs from the next login on. The old secret still verifies tokens
  until they expire (at most 15 min), and the `kid` header selects the right secret.
