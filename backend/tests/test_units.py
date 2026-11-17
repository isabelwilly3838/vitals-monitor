"""Unit tests: resilience primitives, crypto, auth/JWT, config validation, log redaction,
alerting, stream back-pressure. External effects are mocked."""

import asyncio
import json
import time
from unittest.mock import AsyncMock

import jwt
import pytest

from app.config import Env, Settings
from app.errors import AuthError
from app.logging_setup import REDACTED, redact_phi
from app.security.auth import (
    Principal,
    Role,
    decode_token,
    hash_password,
    issue_token,
    verify_password,
)
from app.security.crypto import Cipher, DecryptionError, generate_key
from app.services.anomaly import AlertEngine, RateOfChangeDetector
from app.services.resilience import (
    BreakerState,
    CircuitBreaker,
    CircuitOpenError,
    retry_async,
    supervise,
)
from app.services.store import ResilientStore, SqliteStore
from app.services.stream import Hub, Sample


def run(coro):
    return asyncio.run(coro)


# ------------------------------------------------------------------ retry


def test_retry_succeeds_after_transient_failures():
    fn = AsyncMock(side_effect=[OSError(), OSError(), "ok"])
    assert run(retry_async(fn, attempts=3, base_delay=0)) == "ok"
    assert fn.await_count == 3


def test_retry_gives_up():
    fn = AsyncMock(side_effect=OSError("down"))
    with pytest.raises(OSError):
        run(retry_async(fn, attempts=2, base_delay=0))
    assert fn.await_count == 2


def test_retry_does_not_retry_unlisted_errors():
    fn = AsyncMock(side_effect=KeyError())
    with pytest.raises(KeyError):
        run(retry_async(fn, attempts=5, base_delay=0, retry_on=(OSError,)))
    assert fn.await_count == 1


# ------------------------------------------------------------------ circuit breaker


class FakeClock:
    def __init__(self):
        self.t = 0.0

    def __call__(self):
        return self.t


def test_breaker_full_lifecycle():
    clock = FakeClock()
    br = CircuitBreaker("x", failure_threshold=2, reset_timeout=5, clock=clock)
    bad, good = AsyncMock(side_effect=OSError()), AsyncMock(return_value=1)
    for _ in range(2):
        with pytest.raises(OSError):
            run(br.call(bad))
    assert br.state is BreakerState.open
    with pytest.raises(CircuitOpenError):
        run(br.call(good))
    good.assert_not_awaited()  # short-circuited
    clock.t = 5
    assert br.state is BreakerState.half_open
    with pytest.raises(OSError):
        run(br.call(bad))  # probe fails -> re-open
    assert br.state is BreakerState.open
    clock.t = 10
    assert run(br.call(good)) == 1  # probe succeeds -> closed
    assert br.state is BreakerState.closed


def test_breaker_success_resets_failure_count():
    br = CircuitBreaker("x", failure_threshold=2)
    with pytest.raises(OSError):
        run(br.call(AsyncMock(side_effect=OSError())))
    run(br.call(AsyncMock(return_value=1)))
    with pytest.raises(OSError):
        run(br.call(AsyncMock(side_effect=OSError())))
    assert br.state is BreakerState.closed


def test_resilient_store_buffers_then_drains():
    db = SqliteStore(":memory:", Cipher([generate_key()]))
    rs = ResilientStore(db, attempts=1, breaker=CircuitBreaker("s", 1, 0.0), buffer_size=10)
    res = {"resourceType": "Observation", "id": "a"}
    db.fail_mode = True
    assert run(rs.save(res, "P001", "x", "t")) is False
    assert len(rs.buffer) == 1
    db.fail_mode = False
    assert run(rs.drain()) == 1
    assert db.get("Observation", "a") == res and not rs.buffer


def test_resilient_store_bounded_buffer_counts_drops():
    db = SqliteStore(":memory:", Cipher([generate_key()]))
    db.fail_mode = True
    rs = ResilientStore(db, attempts=1, breaker=CircuitBreaker("s", 100, 1), buffer_size=2)
    for i in range(3):
        run(rs.save({"resourceType": "Observation", "id": str(i)}, None, None, None))
    assert len(rs.buffer) == 2 and rs.dropped == 1


def test_supervisor_restarts_crashed_task():
    calls = []

    async def flaky():
        calls.append(1)
        if len(calls) < 3:
            raise RuntimeError("boom")

    async def main():
        import app.services.resilience as r

        orig = r.asyncio.sleep
        r.asyncio.sleep = AsyncMock()
        try:
            await supervise("t", flaky)
        finally:
            r.asyncio.sleep = orig

    run(main())
    assert len(calls) == 3


# ------------------------------------------------------------------ crypto


def test_encrypt_decrypt_and_rotation():
    old, new = generate_key(), generate_key()
    token = Cipher([old]).encrypt(b"phi")
    rotated_cipher = Cipher([new, old])
    assert rotated_cipher.decrypt(token) == b"phi"  # old data still readable
    rotated = rotated_cipher.rotate(token)
    assert Cipher([new]).decrypt(rotated) == b"phi"  # old key can now be retired
    with pytest.raises(DecryptionError):
        Cipher([generate_key()]).decrypt(token)


def test_cipher_requires_key():
    with pytest.raises(ValueError):
        Cipher([])


# ------------------------------------------------------------------ auth / JWT


def _settings(**kw):
    return Settings(_env_file=None, app_env="test", **kw)


def test_password_hashing():
    h = hash_password("s3cret")
    assert verify_password("s3cret", h) and not verify_password("wrong", h)
    assert not verify_password("x", "not-a-hash")


def test_jwt_roundtrip_and_claims():
    s = _settings()
    tok, ttl = issue_token(Principal("u", [Role.patient], "P001"), s)
    p = decode_token(tok, s)
    assert (p.username, p.roles, p.patient_id, ttl) == ("u", [Role.patient], "P001", 900)


def test_jwt_secret_rotation():
    old = "o" * 40
    new = "n" * 40
    tok, _ = issue_token(Principal("u", [Role.admin]), _settings(jwt_secrets=old))
    assert decode_token(tok, _settings(jwt_secrets=f"{new},{old}")).username == "u"
    with pytest.raises(AuthError):
        decode_token(tok, _settings(jwt_secrets=new))


def test_expired_token_rejected():
    s = _settings()
    claims = {"sub": "u", "roles": [], "iat": 0, "exp": int(time.time()) - 10, "iss": s.jwt_issuer}
    tok = jwt.encode(claims, s.jwt_secret_list[0], algorithm="HS256")
    with pytest.raises(AuthError, match="expired"):
        decode_token(tok, s)


def test_alg_none_rejected():
    s = _settings()
    tok = jwt.encode(
        {"sub": "u", "iat": 0, "exp": 2**31, "iss": s.jwt_issuer}, None, algorithm="none"
    )
    with pytest.raises(AuthError):
        decode_token(tok, s)


def test_rbac_matrix():
    clin = Principal("c", [Role.clinician])
    pat = Principal("p", [Role.patient], "P001")
    dev = Principal("d", [Role.device])
    assert clin.can_access_patient("P002") and not pat.can_access_patient("P002")
    assert pat.can_access_patient("P001") and not dev.can_access_patient("P001")


# ------------------------------------------------------------------ config


def test_prod_refuses_insecure_defaults():
    with pytest.raises(ValueError, match="Unsafe production config"):
        Settings(_env_file=None, app_env="prod")


def test_prod_accepts_hardened_config():
    s = Settings(
        _env_file=None,
        app_env="prod",
        jwt_secrets="x" * 48,
        encryption_keys=generate_key(),
        users_json="[]",
        docs_enabled=False,
    )
    assert s.app_env is Env.prod


def test_secret_file_loading(tmp_path, monkeypatch):
    from app.config import _read_secret_files

    f = tmp_path / "jwt"
    f.write_text("from-file-secret\n")
    monkeypatch.delenv("JWT_SECRETS", raising=False)
    monkeypatch.setenv("JWT_SECRETS_FILE", str(f))
    _read_secret_files()
    import os

    assert os.environ["JWT_SECRETS"] == "from-file-secret"
    monkeypatch.delenv("JWT_SECRETS")


def test_relative_db_path_resolved():
    from pathlib import Path

    p = Path(_settings(database_path="data/x.db").database_path)
    assert p.is_absolute() and p.parts[-2:] == ("data", "x.db")


# ------------------------------------------------------------------ logging


def test_phi_is_redacted_from_logs():
    out = redact_phi(
        None,
        "info",
        {
            "event": "x",
            "password": "p",
            "nested": {"birthDate": "1", "ok": 1},
            "items": [{"value": 37}],
            "path": "/fhir",
        },
    )
    assert out["password"] == REDACTED and out["nested"]["birthDate"] == REDACTED
    assert out["items"][0]["value"] == REDACTED
    assert out["nested"]["ok"] == 1 and out["path"] == "/fhir" and out["event"] == "x"


def test_log_lines_are_json_with_timestamp(capsys):
    from app.logging_setup import configure_logging, get_logger

    configure_logging("INFO", True)
    get_logger("t").info("hello", token="abc")
    line = capsys.readouterr().out.strip().splitlines()[-1]
    rec = json.loads(line)
    assert rec["event"] == "hello" and rec["token"] == REDACTED and rec["timestamp"].endswith("Z")
    assert rec["level"] == "info"


# ------------------------------------------------------------------ alerts / stream


def test_rate_of_change_detector():
    d = RateOfChangeDetector(window=20, z_limit=4)
    assert not any(d.check(120 + (i % 3)) for i in range(15))
    assert d.check(170)


def test_alert_engine_flags():
    e = AlertEngine()
    assert e.evaluate("P", 37, 120, 80) == 0
    assert e.evaluate("P", 38.2, 150, 80) == 1 | 4


def test_slow_subscriber_drops_oldest_not_blocking():
    hub = Hub()

    async def go():
        sub = hub.subscribe(None)
        sub.queue = asyncio.Queue(maxsize=2)
        for i in range(5):
            hub.publish(Sample("P", 0, i, 0, 37, 120, 80))
        return sub

    sub = run(go())
    assert sub.dropped == 3 and sub.queue.get_nowait().seq == 3
