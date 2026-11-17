"""Integration tests through the real ASGI app (auth, RBAC, FHIR REST, WebSocket, audit)."""

import json
from datetime import datetime, timezone

import pytest

from app.services.stream import decode
from tests.conftest import token_for

FHIR = "application/fhir+json"


def post_obs(client, h, obs, ctype=FHIR):
    return client.post(
        "/fhir/Observation", content=json.dumps(obs), headers={**h, "Content-Type": ctype}
    )


# ------------------------------------------------------------------ public / health


def test_capability_statement_is_public_and_valid(client):
    r = client.get("/fhir/metadata")
    assert r.status_code == 200 and r.headers["content-type"].startswith(FHIR)
    from app.fhir.builders import to_fhir_model

    to_fhir_model(r.json())


def test_health_endpoints(client):
    assert client.get("/health/live").json() == {"status": "ok"}
    body = client.get("/health/ready").json()
    assert body["status"] == "ok" and body["checks"]["storage"]["breaker"] == "closed"


def test_security_headers_and_request_id(client):
    r = client.get("/fhir/metadata", headers={"X-Request-ID": "abcdef123456"})
    assert r.headers["x-request-id"] == "abcdef123456"
    assert r.headers["x-content-type-options"] == "nosniff"
    assert r.headers["x-frame-options"] == "DENY"
    assert "frame-ancestors 'none'" in r.headers["content-security-policy"]


def test_malicious_request_id_is_replaced(client):
    r = client.get("/health/live", headers={"X-Request-ID": "<script>"})
    assert r.headers["x-request-id"] != "<script>"


# ------------------------------------------------------------------ authentication


def test_login_wrong_password_returns_operation_outcome(client):
    r = client.post("/auth/token", data={"username": "admin", "password": "nope"})
    assert r.status_code == 401
    assert r.json()["resourceType"] == "OperationOutcome"
    assert r.headers["www-authenticate"] == "Bearer"


def test_unknown_user_same_error_as_wrong_password(client):
    a = client.post("/auth/token", data={"username": "ghost", "password": "x"}).json()
    b = client.post("/auth/token", data={"username": "admin", "password": "x"}).json()
    assert a["issue"][0]["diagnostics"] == b["issue"][0]["diagnostics"]


@pytest.mark.parametrize("auth", [None, "Bearer garbage", "Bearer a.b.c"])
def test_protected_endpoint_requires_valid_token(client, auth):
    h = {"Authorization": auth} if auth else {}
    assert client.get("/fhir/Patient", headers=h).status_code == 401


def test_me_returns_roles(client, headers):
    assert client.get("/auth/me", headers=headers("patient1")).json() == {
        "username": "patient1",
        "roles": ["patient"],
        "patient_id": "P001",
    }


def test_login_rate_limited(client, svc):
    svc.login_limiter.capacity = 2
    svc.login_limiter._buckets.clear()
    for _ in range(2):
        client.post("/auth/token", data={"username": "x", "password": "y"})
    r = client.post("/auth/token", data={"username": "x", "password": "y"})
    assert r.status_code == 429 and r.headers["retry-after"]


# ------------------------------------------------------------------ RBAC


def test_patient_sees_only_own_record(client, headers):
    h = headers("patient1")
    bundle = client.get("/fhir/Patient", headers=h).json()
    assert [e["resource"]["id"] for e in bundle["entry"]] == ["P001"]
    assert client.get("/fhir/Patient/P002", headers=h).status_code == 403
    assert client.get("/fhir/Observation?patient=P002", headers=h).status_code == 403


def test_clinician_sees_all_patients(client, headers):
    bundle = client.get("/fhir/Patient", headers=headers()).json()
    assert bundle["total"] == 3


def test_device_can_write_but_not_read(client, headers, temp_obs):
    h = headers("gateway")
    assert post_obs(client, h, temp_obs()).status_code == 201
    assert client.get("/fhir/Observation", headers=h).status_code == 403


def test_audit_log_admin_only(client, headers):
    assert client.get("/fhir/AuditEvent", headers=headers("clinician")).status_code == 403
    assert client.get("/fhir/AuditEvent", headers=headers("admin")).status_code == 200


# ------------------------------------------------------------------ FHIR REST


def test_create_then_read_and_search(client, headers, bp_obs):
    h = headers()
    r = post_obs(client, h, bp_obs(150, 95))
    assert r.status_code == 201
    created = r.json()
    assert r.headers["location"].endswith(f"/Observation/{created['id']}")
    assert created["interpretation"][0]["coding"][0]["code"] == "H"

    assert client.get(f"/fhir/Observation/{created['id']}", headers=h).json() == created
    bundle = client.get(
        "/fhir/Observation",
        params={"patient": "Patient/P001", "code": "http://loinc.org|85354-9"},
        headers=h,
    ).json()
    assert bundle["resourceType"] == "Bundle" and bundle["type"] == "searchset"
    assert bundle["total"] == 1


def test_search_by_exact_date(client, headers, temp_obs):
    h = headers()
    ts = "2026-03-01T10:00:00.123Z"
    post_obs(client, h, temp_obs(effectiveDateTime=ts))
    hit = client.get("/fhir/Observation", params={"date": f"eq{ts}"}, headers=h).json()
    miss = client.get("/fhir/Observation", params={"date": "ge2026-03-02"}, headers=h).json()
    assert hit["total"] == 1 and miss["total"] == 0
    day = client.get("/fhir/Observation", params={"date": "2026-03-01"}, headers=h).json()
    assert day["total"] == 1


def test_invalid_observation_returns_422_operation_outcome(client, headers, temp_obs):
    r = post_obs(client, headers(), temp_obs(value=99))
    assert r.status_code == 422
    oo = r.json()
    assert oo["resourceType"] == "OperationOutcome"
    assert oo["issue"][0]["expression"] == ["Observation.valueQuantity"]


def test_validate_operation(client, headers, temp_obs):
    ok = client.post(
        "/fhir/Observation/$validate",
        content=json.dumps(temp_obs()),
        headers={**headers(), "Content-Type": FHIR},
    ).json()
    assert ok["issue"][0]["severity"] == "information"


@pytest.mark.parametrize("ctype,body", [("text/plain", "{}"), (FHIR, "{not json")])
def test_bad_content_rejected(client, headers, ctype, body):
    r = client.post("/fhir/Observation", content=body, headers={**headers(), "Content-Type": ctype})
    assert r.status_code == 422


def test_oversize_body_rejected(client, headers):
    r = client.post(
        "/fhir/Observation", content="x" * 70_000, headers={**headers(), "Content-Type": FHIR}
    )
    assert r.status_code == 413


@pytest.mark.parametrize(
    "param",
    [
        {"patient": "P001' OR '1'='1"},
        {"patient": "P001;DROP TABLE resources"},
        {"date": "ge2026-01-01' --"},
        {"code": "1 OR 1=1"},
        {"_count": "99999"},
    ],
)
def test_injection_attempts_rejected(client, headers, param):
    assert client.get("/fhir/Observation", params=param, headers=headers()).status_code == 422


def test_unknown_resource_404_is_operation_outcome(client, headers):
    r = client.get("/fhir/Observation/does-not-exist", headers=headers())
    assert r.status_code == 404 and r.json()["issue"][0]["code"] == "not-found"


def test_actions_are_audited(client, headers, svc):
    h = headers()
    client.get("/fhir/Patient/P001", headers=h)
    client.get("/fhir/Patient/P002", headers=headers("patient1"))
    events = svc.db.list_audit(50)
    actions = [(e["action"], e["outcome"]) for e in events]
    assert ("R", "0") in actions and ("R", "4") in actions
    assert any(e["subtype"][0]["code"] == "login" for e in events)


def test_stored_data_is_encrypted_at_rest(client, headers, svc, temp_obs):
    post_obs(client, headers(), temp_obs(value=39.1))
    raw = svc.db._conn.execute("SELECT data FROM resources WHERE rtype='Observation'").fetchone()
    assert b"39.1" not in raw[0] and b"Observation" not in raw[0]


# ------------------------------------------------------------------ admin


def test_key_rotation_reencrypts(client, headers, svc, temp_obs):
    post_obs(client, headers(), temp_obs())
    before = svc.db._conn.execute("SELECT data FROM resources").fetchall()
    r = client.post("/admin/keys/rotate", headers=headers("admin"))
    assert r.status_code == 200 and r.json()["reencrypted_rows"] >= len(before)
    after = svc.db._conn.execute("SELECT data FROM resources").fetchall()
    assert before != after
    assert client.get("/fhir/Patient/P001", headers=headers()).status_code == 200


def test_chaos_storage_degrades_gracefully(client, headers, temp_obs):
    h, admin = headers(), headers("admin")
    client.post("/admin/chaos/storage?fail=true", headers=admin)
    # writes are accepted and buffered, reads report 503 transient
    for _ in range(4):
        assert post_obs(client, h, temp_obs()).status_code == 201
    assert client.get("/fhir/Patient", headers=h).status_code == 503
    assert client.get("/health/ready").json()["status"] == "degraded"
    svc = client.app.state.services
    assert len(svc.store.buffer) == 4
    client.post("/admin/chaos/storage?fail=false", headers=admin)


# ------------------------------------------------------------------ websocket


def test_websocket_requires_auth(client):
    with client.websocket_connect("/ws/vitals") as ws:
        ws.send_text(json.dumps({"token": "bad"}))
        msg = ws.receive()
        assert msg["type"] == "websocket.close" and msg["code"] == 4401


def test_websocket_streams_binary_frames(client, svc):
    import asyncio

    tok = token_for(client, "clinician")
    with client.websocket_connect("/ws/vitals") as ws:
        ws.send_text(json.dumps({"token": tok}))
        hello = json.loads(ws.receive_text())
        assert hello["type"] == "hello" and hello["frame_bytes"] == 28
        client.portal.call(
            lambda: svc.ingest_sample("P002", datetime.now(timezone.utc), 38.5, 120, 80, "D002")
        )
        s = decode(ws.receive_bytes(), hello["patients"])
        assert s.patient_id == "P002" and s.alerts & 1  # fever flag
        assert round(s.temp_c, 1) == 38.5
    del asyncio


def test_websocket_patient_scoped(client, svc):
    tok = token_for(client, "patient1")
    with client.websocket_connect("/ws/vitals") as ws:
        ws.send_text(json.dumps({"token": tok}))
        assert json.loads(ws.receive_text())["allowed"] == ["P001"]
        now = datetime.now(timezone.utc)
        client.portal.call(lambda: svc.ingest_sample("P002", now, 37, 120, 80, "D"))
        client.portal.call(lambda: svc.ingest_sample("P001", now, 37, 120, 80, "D"))
        assert decode(ws.receive_bytes(), svc.patient_ids).patient_id == "P001"


def test_websocket_fhir_encoding(client, svc):
    tok = token_for(client, "clinician")
    with client.websocket_connect("/ws/vitals") as ws:
        ws.send_text(json.dumps({"token": tok, "encoding": "fhir"}))
        ws.receive_text()
        client.portal.call(
            lambda: svc.ingest_sample("P001", datetime.now(timezone.utc), 37, 120, 80, "D")
        )
        msg = json.loads(ws.receive_text())
        assert [r["code"]["coding"][0]["code"] for r in msg["resources"]] == ["8310-5", "85354-9"]
