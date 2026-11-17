"""Builders for FHIR R4B resources emitted by the backend.

All builders return plain JSON-ready dicts. `to_fhir_model` round-trips them through the
`fhir.resources` R4B pydantic models so every emitted resource is schema-valid.
"""

from __future__ import annotations

import base64
import uuid
from datetime import datetime, timezone
from typing import Any

from fhir.resources.R4B import get_fhir_model_class

from . import codes as c


def now_utc() -> datetime:
    return datetime.now(timezone.utc)


def iso(ts: datetime) -> str:
    return ts.astimezone(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def to_fhir_model(resource: dict[str, Any]):
    """Parse a dict into its fhir.resources model (raises pydantic.ValidationError)."""
    cls = get_fhir_model_class(resource["resourceType"])
    return cls.model_validate(resource)


def _coding(system: str, code: str, display: str | None = None) -> dict[str, str]:
    d = {"system": system, "code": code}
    if display:
        d["display"] = display
    return d


def _quantity(value: float, unit: str) -> dict[str, Any]:
    return {"value": round(float(value), 2), "unit": unit, "system": c.UCUM, "code": unit}


def _interp(code: str) -> list[dict[str, Any]]:
    text = {"H": "High", "L": "Low", "N": "Normal"}[code]
    return [{"coding": [_coding(c.INTERPRETATION, code, text)]}]


def interpret_temp(t: float) -> str:
    return "H" if t >= c.TEMP_HIGH else "L" if t < c.TEMP_LOW else "N"


def interpret_sys(s: float) -> str:
    return "H" if s >= c.SYS_HIGH else "L" if s < c.SYS_LOW else "N"


def interpret_dia(d: float) -> str:
    return "H" if d >= c.DIA_HIGH else "L" if d < c.DIA_LOW else "N"


def _vital_category() -> list[dict[str, Any]]:
    # fresh object per resource: shared mutable structures leak edits between resources
    return [
        {"coding": [_coding(c.OBS_CATEGORY, "vital-signs", "Vital Signs")], "text": "Vital Signs"}
    ]


def _base_obs(
    obs_id: str, patient_id: str, device_id: str, effective: datetime, profile: str, code: str
) -> dict[str, Any]:
    return {
        "resourceType": "Observation",
        "id": obs_id,
        "meta": {"profile": [c.PROFILE_VITALSIGNS, profile], "lastUpdated": iso(now_utc())},
        "status": "final",
        "category": _vital_category(),
        "code": {"coding": [_coding(c.LOINC, code, c.DISPLAY[code])], "text": c.DISPLAY[code]},
        "subject": {"reference": f"Patient/{patient_id}"},
        "device": {"reference": f"Device/{device_id}"},
        "effectiveDateTime": iso(effective),
        "issued": iso(now_utc()),
    }


def body_temperature(
    obs_id: str, patient_id: str, device_id: str, effective: datetime, temp_c: float
) -> dict[str, Any]:
    obs = _base_obs(obs_id, patient_id, device_id, effective, c.PROFILE_BODYTEMP, c.CODE_BODY_TEMP)
    obs["valueQuantity"] = _quantity(temp_c, c.UNIT_CEL)
    obs["interpretation"] = _interp(interpret_temp(temp_c))
    return obs


def blood_pressure(
    obs_id: str,
    patient_id: str,
    device_id: str,
    effective: datetime,
    systolic: float,
    diastolic: float,
) -> dict[str, Any]:
    obs = _base_obs(obs_id, patient_id, device_id, effective, c.PROFILE_BP, c.CODE_BP_PANEL)
    obs["component"] = [
        {
            "code": {"coding": [_coding(c.LOINC, c.CODE_SYSTOLIC, c.DISPLAY[c.CODE_SYSTOLIC])]},
            "valueQuantity": _quantity(systolic, c.UNIT_MMHG),
            "interpretation": _interp(interpret_sys(systolic)),
        },
        {
            "code": {"coding": [_coding(c.LOINC, c.CODE_DIASTOLIC, c.DISPLAY[c.CODE_DIASTOLIC])]},
            "valueQuantity": _quantity(diastolic, c.UNIT_MMHG),
            "interpretation": _interp(interpret_dia(diastolic)),
        },
    ]
    worst = (
        "H"
        if "H" in (interpret_sys(systolic), interpret_dia(diastolic))
        else ("L" if "L" in (interpret_sys(systolic), interpret_dia(diastolic)) else "N")
    )
    obs["interpretation"] = _interp(worst)
    return obs


def patient(patient_id: str, pseudonym: str, gender: str, birth_year: int) -> dict[str, Any]:
    # Pseudonymised demo patients: no real names; identifier is a study code.
    return {
        "resourceType": "Patient",
        "id": patient_id,
        "meta": {"profile": ["http://hl7.org/fhir/StructureDefinition/Patient"]},
        "identifier": [{"system": "urn:vitals-monitor:study-id", "value": pseudonym}],
        "active": True,
        "gender": gender,
        "birthDate": str(birth_year),
    }


def device(device_id: str, patient_id: str, model: str) -> dict[str, Any]:
    return {
        "resourceType": "Device",
        "id": device_id,
        "status": "active",
        "deviceName": [{"name": model, "type": "model-name"}],
        "patient": {"reference": f"Patient/{patient_id}"},
    }


def bundle_searchset(
    entries: list[dict[str, Any]], base_url: str, self_link: str, total: int | None = None
) -> dict[str, Any]:
    return {
        "resourceType": "Bundle",
        "id": str(uuid.uuid4()),
        "type": "searchset",
        "timestamp": iso(now_utc()),
        "total": total if total is not None else len(entries),
        "link": [{"relation": "self", "url": self_link}],
        "entry": [
            {
                "fullUrl": f"{base_url}/{r['resourceType']}/{r['id']}",
                "resource": r,
                "search": {"mode": "match"},
            }
            for r in entries
        ],
    }


def operation_outcome(issues: list[dict[str, str]]) -> dict[str, Any]:
    """issues: [{severity, code, diagnostics, expression?}] with FHIR issue-type codes."""
    out = []
    for i in issues:
        item = {
            "severity": i.get("severity", "error"),
            "code": i.get("code", "processing"),
            "diagnostics": i.get("diagnostics", ""),
        }
        if i.get("expression"):
            item["expression"] = [i["expression"]]
        out.append(item)
    return {"resourceType": "OperationOutcome", "issue": out}


AUDIT_ACTIONS = {
    "create": "C",
    "read": "R",
    "update": "U",
    "delete": "D",
    "search": "E",
    "login": "E",
    "execute": "E",
}


def audit_event(
    *,
    action: str,
    outcome_ok: bool,
    user: str,
    roles: list[str],
    client_ip: str,
    entity_ref: str | None,
    query: str | None,
    subtype: str,
) -> dict[str, Any]:
    """FHIR R4B AuditEvent (RESTful / user-authentication)."""
    is_login = action == "login"
    event: dict[str, Any] = {
        "resourceType": "AuditEvent",
        "id": str(uuid.uuid4()),
        "type": _coding(
            "http://dicom.nema.org/resources/ontology/DCM", "110114", "User Authentication"
        )
        if is_login
        else _coding(
            "http://terminology.hl7.org/CodeSystem/audit-event-type", "rest", "RESTful Operation"
        ),
        "subtype": [_coding("http://hl7.org/fhir/restful-interaction", subtype)],
        "action": AUDIT_ACTIONS.get(action, "E"),
        "recorded": iso(now_utc()),
        "outcome": "0" if outcome_ok else "4",
        "agent": [
            {
                "type": {
                    "coding": [
                        _coding(
                            "http://terminology.hl7.org/CodeSystem/v3-ParticipationType",
                            "IRCP",
                            "information recipient",
                        )
                    ]
                },
                "who": {"identifier": {"system": "urn:vitals-monitor:user", "value": user}},
                "role": [{"text": r} for r in roles] or None,
                "requestor": True,
                "network": {"address": client_ip, "type": "2"},
            }
        ],
        "source": {
            "observer": {"display": "vitals-monitor-backend"},
            "type": [
                _coding(
                    "http://terminology.hl7.org/CodeSystem/security-source-type",
                    "4",
                    "Application Server",
                )
            ],
        },
    }
    if event["agent"][0]["role"] is None:
        del event["agent"][0]["role"]
    if entity_ref or query:
        entity: dict[str, Any] = {}
        if entity_ref:
            entity["what"] = {"reference": entity_ref}
        if query:
            entity["query"] = base64.b64encode(query.encode()).decode()
        event["entity"] = [entity]
    return event
