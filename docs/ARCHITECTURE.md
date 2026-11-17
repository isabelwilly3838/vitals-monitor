# Architecture & design notes

## Layout

```
backend/app/
  config.py            environment-aware settings + prod hardening validation
  logging_setup.py     structlog JSON, request-id/user context, PHI redaction
  errors.py            exception hierarchy + handlers → FHIR OperationOutcome
  fhir/codes.py        LOINC / UCUM / thresholds
  fhir/builders.py     Observation, Patient, Device, Bundle, AuditEvent, OperationOutcome
  fhir/validation.py   schema (fhir.resources R4B) + vital-signs profile + plausibility + XSS
  security/auth.py     JWT issue/verify (rotation via kid), bcrypt, roles → permissions
  security/crypto.py   Fernet / MultiFernet at-rest encryption + rotation
  security/middleware.py request id, access log, security headers, body limit, rate limiter
  services/core.py     service container + ingest pipeline (shared by simulator & REST)
  services/store.py    encrypted SQLite store + ResilientStore (retry, breaker, buffer)
  services/resilience.py retry_async, CircuitBreaker, supervise()
  services/stream.py   binary frame codec + pub/sub hub with bounded per-client queues
  services/anomaly.py  threshold + rate-of-change (z-score) alerting
  services/simulator.py  wearable sensor simulator with clinical episodes
  api/                 routes: auth, fhir, ops (health, admin, websocket)
frontend/
  index.html, css/style.css
  js/api.js            REST client, in-memory token, reconnecting WebSocket
  js/protocol.js       binary frame decoder, Bundle → points
  js/chart.js          reusable D3 real-time chart (band, thresholds, crosshair, click)
  js/app.js            dashboard wiring
```

## Why a binary stream next to FHIR?

FHIR JSON is the system of record and the interoperability format: everything stored and
served by REST is a validated FHIR R4B resource. A 1 Hz feed for 3 patients as FHIR JSON
would be about 1.6 kB per tick per patient. The binary frame carries the same reading in
**28 bytes, roughly 98 % less**.

The frame keeps everything needed to get back to FHIR: the patient index and the
millisecond timestamp. When you click a point, the dashboard runs
`GET /fhir/Observation?patient=…&code=…&date=eq<ts>` to fetch the canonical resource.
Clients that want FHIR on the stream can ask for `"encoding": "fhir"`.

## Data flow per sample

1. The simulator (or `POST /fhir/Observation`) produces readings.
2. The builders create Observations with vital-signs profiles, LOINC codes, UCUM units and
   interpretation codes.
3. `validate_observation` checks every resource, including ones we generate ourselves.
4. `ResilientStore.save` stores the resource. It runs through a circuit breaker and a retry
   with backoff and jitter, wrapped around a thread-offloaded SQLite write. Only the index
   columns are stored in clear text; the full resource is Fernet-encrypted.
   - If storage fails, the write goes into a bounded in-memory buffer and a supervised task
     drains it every 2 s. The live stream is never blocked by storage.
5. The `AlertEngine` sets flags from the thresholds and the systolic z-score. It logs only on
   transitions to avoid flooding the logs.
6. `Hub.publish` pushes to each subscriber's bounded queue. A slow client drops its own
   oldest frames, which isolates faults per client.

## Threat model (STRIDE, summary)

| Threat | Example | Mitigation |
|---|---|---|
| **S**poofing | stolen or forged token | short-lived HS256 JWT with `iss`/`exp`/`kid`; `alg=none` rejected (tested); bcrypt; login rate limit (app + nginx); identical errors for unknown user and wrong password |
| **T**ampering | malicious Observation, SQLi | 3-layer validation, unknown fields rejected, parameterised SQL, id/date regexes, Fernet's HMAC detects tampering at rest |
| **R**epudiation | "I never viewed that record" | FHIR AuditEvent for login, read, search and create (success and failure), stored encrypted, admin-only access |
| **I**nformation disclosure | PHI in logs, token in URL, XSS | PHI redaction processor, WS token sent in the first message (not the query string), in-memory token, CSP, the frontend writes text only (no `innerHTML`), encryption at rest, TLS + HSTS, patient-scoped RBAC |
| **D**enial of service | large bodies, slow WS clients, login floods | 64 kB body limit (app + nginx), nginx `limit_req`, bounded queues, `_count` capped |
| **E**levation of privilege | patient reading another patient's data | permission matrix per role plus a per-patient check on every read, search and stream subscription (tested) |

## Configuration precedence

defaults → `config/<APP_ENV>.env` → `.env` → environment variables → `NAME_FILE` secret files.

In `prod` the app refuses to start if any of these hold:

- JWT secrets are missing, shorter than 32 characters, or the dev default
- no encryption key is set
- `USERS_JSON` is not set
- CORS is `*`
- API docs are enabled

## Key rotation

- **Encryption:** `scripts/rotate_keys.py` puts a new key first; after a restart, call
  `POST /admin/keys/rotate` to re-encrypt every row (MultiFernet). Then run
  `rotate_keys.py --prune`.
- **JWT:** a new secret signs from the next login on. Tokens signed with the old secret stay
  valid until they expire (at most 15 min), and the `kid` header selects the verifying secret.
