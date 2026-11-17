"""Unit tests: FHIR builders and the 3-layer validator (core + edge cases)."""

import copy
import json
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from app.fhir import builders as b
from app.fhir import codes as c
from app.fhir.validation import validate_observation

KNOWN = {"P001", "P002", "P003"}
GOLDEN = Path(__file__).parent / "golden"


def codes_of(issues):
    return {i["code"] for i in issues}


# ------------------------------------------------------------------ builders


@pytest.mark.parametrize(
    "res",
    [
        b.body_temperature("o1", "P001", "D001", datetime.now(timezone.utc), 37.2),
        b.blood_pressure("o2", "P001", "D001", datetime.now(timezone.utc), 130, 85),
        b.patient("P001", "STUDY-1", "female", 1970),
        b.device("D001", "P001", "x"),
        b.operation_outcome([{"diagnostics": "x"}]),
        b.audit_event(
            action="read",
            outcome_ok=True,
            user="u",
            roles=["admin"],
            client_ip="::1",
            entity_ref="Patient/P001",
            query="a=b",
            subtype="read",
        ),
    ],
)
def test_every_built_resource_is_schema_valid(res):
    b.to_fhir_model(res)  # raises on any schema violation


def test_bundle_wraps_entries_with_full_urls():
    o = b.body_temperature("o1", "P001", "D001", datetime.now(timezone.utc), 37)
    bundle = b.bundle_searchset([o], "http://h/fhir", "http://h/fhir/Observation", total=7)
    b.to_fhir_model(bundle)
    assert bundle["total"] == 7
    assert bundle["entry"][0]["fullUrl"] == "http://h/fhir/Observation/o1"


@pytest.mark.parametrize("t,expected", [(37.0, "N"), (38.0, "H"), (34.9, "L"), (35.0, "N")])
def test_temperature_interpretation_boundaries(t, expected):
    o = b.body_temperature("o", "P001", "D", datetime.now(timezone.utc), t)
    assert o["interpretation"][0]["coding"][0]["code"] == expected


def test_bp_interpretation_takes_worst_component():
    o = b.blood_pressure("o", "P001", "D", datetime.now(timezone.utc), 120, 95)
    assert o["interpretation"][0]["coding"][0]["code"] == "H"
    assert o["component"][1]["interpretation"][0]["coding"][0]["code"] == "H"


def test_audit_event_login_uses_dicom_type():
    ev = b.audit_event(
        action="login",
        outcome_ok=False,
        user="x",
        roles=[],
        client_ip="1",
        entity_ref=None,
        query=None,
        subtype="login",
    )
    assert ev["type"]["code"] == "110114" and ev["outcome"] == "4"
    assert "role" not in ev["agent"][0]


# ------------------------------------------------------------------ regression (golden files)


def _normalise(res):
    r = copy.deepcopy(res)
    r["meta"].pop("lastUpdated")
    r.pop("issued")
    return r


@pytest.mark.parametrize(
    "name,factory",
    [
        ("bodytemp", lambda ts: b.body_temperature("P001-t-1", "P001", "D001", ts, 38.4)),
        ("bp", lambda ts: b.blood_pressure("P001-bp-1", "P001", "D001", ts, 142.5, 91)),
    ],
)
def test_output_matches_golden_file(name, factory):
    """Regression guard: any change to emitted FHIR JSON must be deliberate.
    Regenerate with UPDATE_GOLDEN=1 pytest."""
    import os

    ts = datetime(2026, 1, 1, 12, 0, tzinfo=timezone.utc)
    got = _normalise(factory(ts))
    path = GOLDEN / f"{name}.json"
    if os.getenv("UPDATE_GOLDEN") or not path.exists():
        path.write_text(json.dumps(got, indent=2) + "\n")
    assert got == json.loads(path.read_text())


# ------------------------------------------------------------------ validator: happy paths


def test_valid_temperature_passes(temp_obs):
    assert validate_observation(temp_obs(), KNOWN) == []


def test_valid_bp_passes(bp_obs):
    assert validate_observation(bp_obs(), KNOWN) == []


def test_naive_but_offset_datetime_passes(temp_obs):
    o = temp_obs(effectiveDateTime="2026-01-01T10:00:00+02:00")
    assert validate_observation(o, KNOWN) == []


# ------------------------------------------------------------------ validator: edge cases


@pytest.mark.parametrize("body", [None, [], "x", 1, {"resourceType": "Patient"}])
def test_non_observation_bodies_rejected(body):
    assert codes_of(validate_observation(body)) == {"structure"}


def test_unknown_field_rejected_by_schema(temp_obs):
    assert "structure" in codes_of(validate_observation(temp_obs(evil=1), KNOWN))


def test_wrong_type_rejected_by_schema(temp_obs):
    o = temp_obs()
    o["valueQuantity"]["value"] = "hot"
    assert "structure" in codes_of(validate_observation(o, KNOWN))


def test_invalid_status_rejected(temp_obs):
    # fhir.resources does not enforce the required binding; our profile layer does.
    assert "code-invalid" in codes_of(validate_observation(temp_obs(status="bogus"), KNOWN))


def test_missing_vital_signs_category(temp_obs):
    o = temp_obs()
    o["category"][0]["coding"][0]["code"] = "laboratory"
    assert "required" in codes_of(validate_observation(o, KNOWN))


def test_unknown_patient_rejected(temp_obs):
    assert "not-found" in codes_of(validate_observation(temp_obs(patient="P999"), KNOWN))


def test_subject_must_be_patient(temp_obs):
    o = temp_obs(subject={"reference": "Group/1"})
    assert "required" in codes_of(validate_observation(o, KNOWN))


def test_future_timestamp_rejected(temp_obs):
    future = (datetime.now(timezone.utc) + timedelta(hours=1)).isoformat()
    assert "value" in codes_of(validate_observation(temp_obs(effectiveDateTime=future), KNOWN))


def test_missing_timezone_rejected(temp_obs):
    o = temp_obs()
    o["effectiveDateTime"] = "2026-01-01"  # valid FHIR date but no instant
    assert validate_observation(o, KNOWN)


@pytest.mark.parametrize("value", [24.9, 45.1, -1, 1e9])
def test_implausible_temperature_rejected(temp_obs, value):
    assert "value" in codes_of(validate_observation(temp_obs(value=value), KNOWN))


@pytest.mark.parametrize("value", [25.0, 45.0])
def test_temperature_range_is_inclusive(temp_obs, value):
    assert validate_observation(temp_obs(value=value), KNOWN) == []


def test_wrong_unit_rejected(temp_obs):
    o = temp_obs()
    o["valueQuantity"].update(code="[degF]", unit="[degF]")
    assert "code-invalid" in codes_of(validate_observation(o, KNOWN))


def test_missing_value_rejected(temp_obs):
    o = temp_obs()
    del o["valueQuantity"]
    assert "required" in codes_of(validate_observation(o, KNOWN))


def test_systolic_must_exceed_diastolic(bp_obs):
    assert "value" in codes_of(validate_observation(bp_obs(80, 90), KNOWN))


def test_missing_bp_component(bp_obs):
    o = bp_obs()
    o["component"] = o["component"][:1]
    assert "required" in codes_of(validate_observation(o, KNOWN))


def test_unsupported_loinc_code(temp_obs):
    o = temp_obs()
    o["code"]["coding"][0]["code"] = "8867-4"  # heart rate
    assert "not-supported" in codes_of(validate_observation(o, KNOWN))


@pytest.mark.parametrize(
    "payload",
    [
        "<script>alert(1)</script>",
        "<img src=x onerror=alert(1)>",
        "javascript:alert(1)",
        "x\x00y",
        "<svg/onload=alert(1)>",
    ],
)
def test_xss_and_control_chars_rejected(temp_obs, payload):
    o = temp_obs()
    o["code"]["text"] = payload
    assert "security" in codes_of(validate_observation(o, KNOWN))


def test_sql_injection_string_is_just_data_not_executed(temp_obs):
    o = temp_obs(subject={"reference": "Patient/P001' OR '1'='1"})
    # rejected as unknown patient - never reaches SQL un-parameterised
    assert "not-found" in codes_of(validate_observation(o, KNOWN))


def test_deeply_nested_input_does_not_crash(temp_obs):
    deep = cur = {}
    for _ in range(50):
        cur["x"] = {}
        cur = cur["x"]
    o = temp_obs()
    o["code"]["extension"] = [deep]
    assert validate_observation(o, KNOWN)


def test_constants_match_loinc():
    assert (c.CODE_BODY_TEMP, c.CODE_SYSTOLIC, c.CODE_DIASTOLIC) == ("8310-5", "8480-6", "8462-4")
