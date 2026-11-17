import os

os.environ["APP_ENV"] = "test"  # must happen before the app is imported

from datetime import datetime, timezone  # noqa: E402

import pytest  # noqa: E402
from fastapi.testclient import TestClient  # noqa: E402

from app.config import get_settings  # noqa: E402
from app.fhir import builders as b  # noqa: E402
from app.main import create_app  # noqa: E402

PASSWORDS = {
    "admin": "admin-dev-pass",
    "clinician": "clinician-dev-pass",
    "patient1": "patient-dev-pass",
    "gateway": "device-dev-pass",
}


@pytest.fixture
def client():
    with TestClient(create_app(get_settings())) as c:
        yield c


@pytest.fixture
def svc(client):
    return client.app.state.services


def token_for(client, user: str) -> str:
    r = client.post("/auth/token", data={"username": user, "password": PASSWORDS[user]})
    assert r.status_code == 200, r.text
    return r.json()["access_token"]


@pytest.fixture
def headers(client):
    def _h(user: str = "clinician") -> dict:
        return {"Authorization": f"Bearer {token_for(client, user)}"}

    return _h


@pytest.fixture
def temp_obs():
    def _make(value=37.0, patient="P001", **over):
        o = b.body_temperature("t1", patient, "D001", datetime.now(timezone.utc), value)
        o.update(over)
        return o

    return _make


@pytest.fixture
def bp_obs():
    def _make(sys_=120.0, dia=80.0, patient="P001"):
        return b.blood_pressure("bp1", patient, "D001", datetime.now(timezone.utc), sys_, dia)

    return _make
