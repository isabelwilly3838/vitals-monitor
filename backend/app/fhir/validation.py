"""Three-layer validation for inbound Observations.

1. Structural:   full FHIR R4B schema via fhir.resources (types, cardinality, unknown fields).
2. Profile:      Vital Signs profile rules (status binding, category, LOINC code, UCUM units,
                 subject/effective present, no future timestamps).
3. Plausibility: physiological range checks (sensor-fault detection) and systolic > diastolic.

Plus a security layer rejecting markup / control characters in any string, which blocks
stored-XSS payloads before they reach storage or the dashboard.

Every failure is returned as a FHIR OperationOutcome issue; nothing here raises on bad input.
"""

from __future__ import annotations

import re
from datetime import datetime, timedelta, timezone
from typing import Any

from pydantic import ValidationError

from . import codes as c
from .builders import to_fhir_model

FHIR_ID_RE = re.compile(r"^[A-Za-z0-9\-.]{1,64}$")
MARKUP_RE = re.compile(r"<\s*/?\s*[a-zA-Z!?]|javascript:|on\w+\s*=", re.IGNORECASE)
CONTROL_RE = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]")
MAX_FUTURE_SKEW = timedelta(minutes=5)
MAX_DEPTH = 12

Issue = dict[str, str]


def _issue(
    code: str, diagnostics: str, expression: str | None = None, severity: str = "error"
) -> Issue:
    i = {"severity": severity, "code": code, "diagnostics": diagnostics}
    if expression:
        i["expression"] = expression
    return i


def scan_strings(obj: Any, path: str = "Observation", depth: int = 0) -> list[Issue]:
    if depth > MAX_DEPTH:
        return [_issue("too-costly", "Resource nesting too deep", path)]
    issues: list[Issue] = []
    if isinstance(obj, str):
        if MARKUP_RE.search(obj):
            issues.append(_issue("security", "Markup or script content is not allowed", path))
        if CONTROL_RE.search(obj):
            issues.append(_issue("security", "Control characters are not allowed", path))
    elif isinstance(obj, dict):
        for k, v in obj.items():
            issues += scan_strings(v, f"{path}.{k}", depth + 1)
    elif isinstance(obj, list):
        for idx, v in enumerate(obj):
            issues += scan_strings(v, f"{path}[{idx}]", depth + 1)
    return issues


def _codes(concept: dict | None, system: str) -> set[str]:
    if not concept:
        return set()
    return {cd.get("code") for cd in concept.get("coding", []) if cd.get("system") == system}


def _check_quantity(
    q: dict | None, unit: str, rng: tuple[float, float], expr: str, label: str
) -> tuple[list[Issue], float | None]:
    if not q:
        return [_issue("required", f"{label} valueQuantity is required", expr)], None
    issues = []
    if q.get("system") != c.UCUM or q.get("code") != unit:
        issues.append(_issue("code-invalid", f"{label} must use UCUM unit '{unit}'", expr))
    v = q.get("value")
    if not isinstance(v, (int, float)) or isinstance(v, bool):
        issues.append(_issue("value", f"{label} value must be numeric", expr))
        return issues, None
    lo, hi = rng
    if not lo <= float(v) <= hi:
        issues.append(_issue("value", f"{label} {v} outside plausible range [{lo}, {hi}]", expr))
    return issues, float(v)


def _parse_dt(s: Any) -> datetime | None:
    if not isinstance(s, str):
        return None
    try:
        dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
    except ValueError:
        return None
    return dt if dt.tzinfo else None


def validate_observation(data: Any, known_patients: set[str] | None = None) -> list[Issue]:
    """Return a list of OperationOutcome issues; empty list == valid."""
    if not isinstance(data, dict):
        return [_issue("structure", "Body must be a JSON object")]
    if data.get("resourceType") != "Observation":
        return [_issue("structure", "resourceType must be 'Observation'", "resourceType")]

    issues = scan_strings(data)
    if issues:
        return issues

    # --- 1. structural (FHIR R4B schema)
    try:
        to_fhir_model(data)
    except ValidationError as e:
        for err in e.errors()[:20]:
            loc = ".".join(str(p) for p in err.get("loc", ()) if p != "__root__")
            issues.append(
                _issue(
                    "structure",
                    err.get("msg", "invalid")[:200],
                    f"Observation.{loc}" if loc else None,
                )
            )
        return issues
    except Exception:  # noqa: BLE001 - malformed input must never 500
        return [_issue("structure", "Resource could not be parsed as FHIR R4B Observation")]

    # --- 2. profile (vital signs)
    if "id" in data and not FHIR_ID_RE.match(str(data["id"])):
        issues.append(_issue("value", "id does not match FHIR id pattern", "Observation.id"))
    if data.get("status") not in c.OBS_STATUS:
        issues.append(
            _issue("code-invalid", "status is not in the required value set", "Observation.status")
        )
    cats = set()
    for cat in data.get("category", []) or []:
        cats |= _codes(cat, c.OBS_CATEGORY)
    if "vital-signs" not in cats:
        issues.append(
            _issue("required", "category must include 'vital-signs'", "Observation.category")
        )

    ref = (data.get("subject") or {}).get("reference", "")
    if not ref.startswith("Patient/"):
        issues.append(_issue("required", "subject must reference a Patient", "Observation.subject"))
    elif known_patients is not None and ref.split("/", 1)[1] not in known_patients:
        issues.append(
            _issue("not-found", "subject references an unknown Patient", "Observation.subject")
        )

    eff = _parse_dt(data.get("effectiveDateTime"))
    if eff is None:
        issues.append(
            _issue(
                "required",
                "effectiveDateTime with timezone is required",
                "Observation.effectiveDateTime",
            )
        )
    elif eff > datetime.now(timezone.utc) + MAX_FUTURE_SKEW:
        issues.append(
            _issue("value", "effectiveDateTime is in the future", "Observation.effectiveDateTime")
        )

    # --- 3. code-specific checks + plausibility
    loinc = _codes(data.get("code"), c.LOINC)
    if c.CODE_BODY_TEMP in loinc:
        iss, _ = _check_quantity(
            data.get("valueQuantity"),
            c.UNIT_CEL,
            c.PLAUSIBLE["temp_c"],
            "Observation.valueQuantity",
            "Body temperature",
        )
        issues += iss
    elif c.CODE_BP_PANEL in loinc:
        comps = {}
        for idx, comp in enumerate(data.get("component", []) or []):
            for code in _codes(comp.get("code"), c.LOINC):
                comps[code] = (idx, comp)
        values = {}
        for code, key, label in (
            (c.CODE_SYSTOLIC, "systolic", "Systolic"),
            (c.CODE_DIASTOLIC, "diastolic", "Diastolic"),
        ):
            if code not in comps:
                issues.append(
                    _issue(
                        "required",
                        f"{label} component ({code}) is required",
                        "Observation.component",
                    )
                )
                continue
            idx, comp = comps[code]
            iss, v = _check_quantity(
                comp.get("valueQuantity"),
                c.UNIT_MMHG,
                c.PLAUSIBLE[key],
                f"Observation.component[{idx}].valueQuantity",
                label,
            )
            issues += iss
            values[key] = v
        if (
            values.get("systolic") is not None
            and values.get("diastolic") is not None
            and values["systolic"] <= values["diastolic"]
        ):
            issues.append(
                _issue("value", "Systolic must be greater than diastolic", "Observation.component")
            )
    else:
        issues.append(
            _issue(
                "not-supported",
                f"Unsupported code; supported LOINC: {c.CODE_BODY_TEMP}, {c.CODE_BP_PANEL}",
                "Observation.code",
            )
        )
    return issues


def validate_search_id(value: str) -> bool:
    return bool(FHIR_ID_RE.match(value))
