"""FHIR R4B RESTful endpoints (subset): metadata, Patient, Device, Observation, AuditEvent."""

from __future__ import annotations

import asyncio
import json
import re
from typing import Any

from fastapi import APIRouter, Depends, Query, Request
from fastapi.responses import JSONResponse

from ..errors import FHIR_JSON, ForbiddenError, NotFoundError, ServiceUnavailable, ValidationFailed
from ..fhir import builders as b
from ..fhir import codes as c
from ..fhir.validation import validate_observation, validate_search_id
from ..security.auth import Perm, Principal, require
from ..services.core import Services, normalize_dt
from ..services.store import StorageError
from .deps import client_ip, services

router = APIRouter(prefix="/fhir", tags=["fhir"])

DATE_PARAM_RE = re.compile(r"^(eq|ge|le|gt|lt)?(\d{4}-\d{2}-\d{2}(T[\d:.]+(Z|[+-]\d{2}:\d{2})?)?)$")
SUPPORTED_CODES = {c.CODE_BODY_TEMP, c.CODE_BP_PANEL}


def fhir(body: dict, status: int = 200, headers: dict | None = None) -> JSONResponse:
    return JSONResponse(body, status_code=status, media_type=FHIR_JSON, headers=headers)


def _base(request: Request) -> str:
    return str(request.base_url).rstrip("/") + "/fhir"


async def _db(fn, *args, **kwargs):
    try:
        return await asyncio.to_thread(fn, *args, **kwargs)
    except StorageError as e:
        raise ServiceUnavailable("Storage temporarily unavailable, retry shortly") from e


def _bad(code: str, msg: str, expr: str) -> ValidationFailed:
    return ValidationFailed(
        msg, [{"severity": "error", "code": code, "diagnostics": msg, "expression": expr}]
    )


# --------------------------------------------------------------------------- metadata


@router.get("/metadata")
async def capability_statement():
    obs_search = [
        {"name": n, "type": t}
        for n, t in (
            ("patient", "reference"),
            ("code", "token"),
            ("date", "date"),
            ("_count", "number"),
        )
    ]
    return fhir(
        {
            "resourceType": "CapabilityStatement",
            "status": "active",
            "date": "2026-11-17",
            "kind": "instance",
            "fhirVersion": "4.3.0",
            "format": ["application/fhir+json"],
            "implementation": {"description": "Vitals Monitor FHIR facade"},
            "rest": [
                {
                    "mode": "server",
                    "security": {
                        "cors": False,
                        "service": [
                            {
                                "coding": [
                                    {
                                        "system": "http://terminology.hl7.org/CodeSystem/restful-security-service",
                                        "code": "OAuth",
                                        "display": "OAuth",
                                    }
                                ]
                            }
                        ],
                        "description": "Bearer JWT from POST /auth/token; RBAC roles admin, "
                        "clinician, patient, device",
                    },
                    "resource": [
                        {
                            "type": "Patient",
                            "interaction": [{"code": "read"}, {"code": "search-type"}],
                        },
                        {"type": "Device", "interaction": [{"code": "read"}]},
                        {
                            "type": "Observation",
                            "supportedProfile": [c.PROFILE_BODYTEMP, c.PROFILE_BP],
                            "interaction": [
                                {"code": "read"},
                                {"code": "search-type"},
                                {"code": "create"},
                            ],
                            "searchParam": obs_search,
                            "operation": [
                                {
                                    "name": "validate",
                                    "definition": "http://hl7.org/fhir/OperationDefinition/"
                                    "Resource-validate",
                                }
                            ],
                        },
                        {"type": "AuditEvent", "interaction": [{"code": "search-type"}]},
                    ],
                }
            ],
        }
    )


# --------------------------------------------------------------------------- Patient


@router.get("/Patient")
async def search_patients(
    request: Request,
    svc: Services = Depends(services),
    p: Principal = Depends(require(Perm.read_patient)),
):
    rows = await _db(svc.db.list_type, "Patient")
    rows = [r for r in rows if p.can_access_patient(r["id"])]
    svc.audit(
        action="search",
        ok=True,
        principal=p,
        client_ip=client_ip(request),
        query=str(request.url.query),
        subtype="search-type",
    )
    return fhir(b.bundle_searchset(rows, _base(request), str(request.url)))


@router.get("/Patient/{pid}")
async def read_patient(
    pid: str,
    request: Request,
    svc: Services = Depends(services),
    p: Principal = Depends(require(Perm.read_patient)),
):
    if not validate_search_id(pid):
        raise _bad("value", "Invalid id", "Patient.id")
    if not p.can_access_patient(pid):
        svc.audit(
            action="read",
            ok=False,
            principal=p,
            client_ip=client_ip(request),
            entity_ref=f"Patient/{pid}",
        )
        raise ForbiddenError("Not permitted to access this patient")
    res = await _db(svc.db.get, "Patient", pid)
    if not res:
        raise NotFoundError("Patient not found")
    svc.audit(
        action="read",
        ok=True,
        principal=p,
        client_ip=client_ip(request),
        entity_ref=f"Patient/{pid}",
    )
    return fhir(res)


@router.get("/Device/{did}")
async def read_device(
    did: str,
    request: Request,
    svc: Services = Depends(services),
    p: Principal = Depends(require(Perm.read_patient)),
):
    if not validate_search_id(did):
        raise _bad("value", "Invalid id", "Device.id")
    res = await _db(svc.db.get, "Device", did)
    if not res:
        raise NotFoundError("Device not found")
    owner = res.get("patient", {}).get("reference", "/").split("/")[-1]
    if not p.can_access_patient(owner):
        raise ForbiddenError("Not permitted to access this device")
    return fhir(res)


# --------------------------------------------------------------------------- Observation


def _parse_dates(values: list[str]) -> tuple[str | None, str | None]:
    ge = le = None
    for v in values:
        m = DATE_PARAM_RE.match(v)
        if not m:
            raise _bad("value", "date must be [eq|ge|le|gt|lt]YYYY-MM-DD[Thh:mm:ss[.sss]Z]", "date")
        prefix, raw = m.group(1) or "eq", m.group(2)
        try:
            norm = normalize_dt(raw if "T" in raw else raw + "T00:00:00Z")
        except ValueError as e:
            raise _bad("value", "Invalid date value", "date") from e
        if prefix in ("ge", "gt", "eq"):
            ge = norm
        if prefix in ("le", "lt", "eq"):
            le = norm if "T" in raw else normalize_dt(raw + "T23:59:59.999Z")
    return ge, le


@router.get("/Observation")
async def search_observations(
    request: Request,
    patient: str | None = Query(None, max_length=72),
    code: str | None = Query(None, max_length=64),
    date: list[str] = Query(default_factory=list, max_length=4),
    count: int = Query(100, alias="_count", ge=1, le=2000),
    svc: Services = Depends(services),
    p: Principal = Depends(require(Perm.read_obs)),
):
    pid = patient.removeprefix("Patient/") if patient else None
    if pid is not None and not validate_search_id(pid):
        raise _bad("value", "Invalid patient id", "patient")
    if pid is None and not p.can_access_patient("*"):
        pid = p.patient_id  # patients implicitly scoped to themselves
    if pid is not None and not p.can_access_patient(pid):
        svc.audit(
            action="search",
            ok=False,
            principal=p,
            client_ip=client_ip(request),
            query=str(request.url.query),
            subtype="search-type",
        )
        raise ForbiddenError("Not permitted to access this patient")
    loinc = None
    if code:
        loinc = code.split("|")[-1]
        if loinc not in SUPPORTED_CODES:
            raise _bad("not-supported", "Unsupported code", "code")
    ge, le = _parse_dates(date)
    rows, total = await _db(
        svc.db.search_observations, patient_id=pid, code=loinc, date_ge=ge, date_le=le, limit=count
    )
    svc.audit(
        action="search",
        ok=True,
        principal=p,
        client_ip=client_ip(request),
        entity_ref=f"Patient/{pid}" if pid else None,
        query=str(request.url.query),
        subtype="search-type",
    )
    return fhir(b.bundle_searchset(rows, _base(request), str(request.url), total))


@router.get("/Observation/{oid}")
async def read_observation(
    oid: str,
    request: Request,
    svc: Services = Depends(services),
    p: Principal = Depends(require(Perm.read_obs)),
):
    if not validate_search_id(oid):
        raise _bad("value", "Invalid id", "Observation.id")
    res = await _db(svc.db.get, "Observation", oid)
    if not res:
        raise NotFoundError("Observation not found")
    pid = res["subject"]["reference"].split("/", 1)[1]
    if not p.can_access_patient(pid):
        raise ForbiddenError("Not permitted to access this observation")
    svc.audit(
        action="read",
        ok=True,
        principal=p,
        client_ip=client_ip(request),
        entity_ref=f"Observation/{oid}",
    )
    return fhir(res)


async def _json_body(request: Request) -> Any:
    ctype = request.headers.get("content-type", "").split(";")[0].strip()
    if ctype not in ("application/fhir+json", "application/json"):
        raise _bad("not-supported", "Content-Type must be application/fhir+json", "header")
    try:
        return json.loads(await request.body())
    except (json.JSONDecodeError, UnicodeDecodeError) as e:
        raise _bad("structure", "Body is not valid JSON", "body") from e


@router.post("/Observation", status_code=201)
async def create_observation(
    request: Request,
    svc: Services = Depends(services),
    p: Principal = Depends(require(Perm.write_obs)),
):
    data = await _json_body(request)
    try:
        created = await svc.ingest_observation(data, p)
    except ValidationFailed:
        svc.audit(
            action="create", ok=False, principal=p, client_ip=client_ip(request), subtype="create"
        )
        raise
    svc.audit(
        action="create",
        ok=True,
        principal=p,
        client_ip=client_ip(request),
        entity_ref=f"Observation/{created['id']}",
        subtype="create",
    )
    return fhir(created, 201, {"Location": f"{_base(request)}/Observation/{created['id']}"})


@router.post("/Observation/$validate")
async def validate_only(
    request: Request,
    svc: Services = Depends(services),
    p: Principal = Depends(require(Perm.write_obs)),
):
    data = await _json_body(request)
    issues = validate_observation(data, set(svc.patient_ids))
    if not issues:
        issues = [
            {
                "severity": "information",
                "code": "informational",
                "diagnostics": "Validation successful",
            }
        ]
    return fhir(b.operation_outcome(issues), 200)


# --------------------------------------------------------------------------- AuditEvent


@router.get("/AuditEvent")
async def search_audit(
    request: Request,
    count: int = Query(100, alias="_count", ge=1, le=1000),
    svc: Services = Depends(services),
    p: Principal = Depends(require(Perm.read_audit)),
):
    rows = await asyncio.to_thread(svc.db.list_audit, count)
    return fhir(b.bundle_searchset(rows, _base(request), str(request.url)))
