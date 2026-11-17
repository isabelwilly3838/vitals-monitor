"""Health probes, admin operations and the real-time WebSocket stream."""

from __future__ import annotations

import asyncio
import json
import math
from datetime import datetime, timezone

from fastapi import APIRouter, Depends, Request, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse

from ..config import Env, Settings, get_settings
from ..errors import AppError, ForbiddenError
from ..fhir import builders as b
from ..logging_setup import get_logger
from ..security.auth import Perm, Principal, decode_token, require
from ..services.core import Services
from ..services.stream import encode
from .deps import client_ip, services

log = get_logger("ops")
router = APIRouter()


# --------------------------------------------------------------------------- health


@router.get("/health/live", tags=["ops"])
async def live():
    return {"status": "ok"}


@router.get("/health/ready", tags=["ops"])
async def ready(svc: Services = Depends(services)):
    checks = {"storage": svc.store.status()}
    try:
        await asyncio.to_thread(svc.db.ping)
        checks["storage"]["ping"] = "ok"
    except Exception:  # noqa: BLE001
        checks["storage"]["ping"] = "fail"
    checks["stream"] = {"subscribers": svc.hub.subscriber_count, "published": svc.hub.published}
    # Degraded (buffering) still serves the live stream -> report 200 with status=degraded.
    degraded = checks["storage"]["ping"] != "ok" or checks["storage"]["breaker"] != "closed"
    return JSONResponse(
        {"status": "degraded" if degraded else "ok", "checks": checks}, status_code=200
    )


# --------------------------------------------------------------------------- admin


@router.post("/admin/keys/rotate", tags=["admin"])
async def rotate_keys(
    request: Request,
    svc: Services = Depends(services),
    p: Principal = Depends(require(Perm.admin_keys)),
):
    n = await asyncio.to_thread(svc.db.rotate_keys)
    svc.audit(
        action="execute",
        ok=True,
        principal=p,
        client_ip=client_ip(request),
        subtype="operation",
        entity_ref=None,
        query="keys/rotate",
    )
    log.warning("encryption_keys_rotated", rows=n, keys=svc.cipher.key_count)
    return {"reencrypted_rows": n, "active_keys": svc.cipher.key_count}


@router.post("/admin/chaos/storage", tags=["admin"])
async def chaos_storage(
    fail: bool,
    svc: Services = Depends(services),
    settings: Settings = Depends(get_settings),
    p: Principal = Depends(require(Perm.admin_keys)),
):
    """Fault injection to demonstrate breaker + buffering. Disabled in prod."""
    if settings.app_env is Env.prod:
        raise ForbiddenError("Chaos endpoints are disabled in production")
    svc.db.fail_mode = fail
    log.warning("chaos_storage_toggled", fail=fail)
    return {"storage_fail_mode": fail, **svc.store.status()}


# --------------------------------------------------------------------------- websocket


@router.websocket("/ws/vitals")
async def vitals_stream(ws: WebSocket):
    """Protocol:
      client -> {"token": "<jwt>", "encoding": "binary"|"fhir", "patients": ["P001"]?}
      server -> {"type":"hello", "patients":[...], "frame_bytes":28}   (text)
      server -> binary frames (see services/stream.py) | FHIR JSON text messages
    Token goes in the first message, not the URL, so it never lands in proxy logs."""
    svc: Services = ws.app.state.services
    settings = get_settings()
    await ws.accept()
    try:
        raw = await asyncio.wait_for(ws.receive_text(), timeout=settings.ws_auth_timeout_s)
        msg = json.loads(raw)
        principal = decode_token(str(msg.get("token", "")), settings)
        if not principal.has(Perm.stream):
            raise ForbiddenError("Role may not subscribe to the stream")
    except (TimeoutError, json.JSONDecodeError, AppError, AttributeError) as e:
        log.warning("ws_auth_failed", reason=type(e).__name__)
        await ws.close(code=4401, reason="unauthorized")
        return
    except WebSocketDisconnect:
        return

    allowed = [pid for pid in svc.patient_ids if principal.can_access_patient(pid)]
    wanted = msg.get("patients")
    if isinstance(wanted, list):
        allowed = [pid for pid in allowed if pid in wanted]
    encoding = "fhir" if msg.get("encoding") == "fhir" else "binary"
    sub = svc.hub.subscribe(set(allowed))
    log.info("ws_subscribed", user=principal.username, patients=allowed, encoding=encoding)
    await ws.send_text(
        json.dumps(
            {
                "type": "hello",
                "encoding": encoding,
                "patients": svc.patient_ids,
                "allowed": allowed,
                "frame_bytes": 28,
            }
        )
    )

    async def pump():
        while True:
            s = await sub.queue.get()
            if encoding == "binary":
                await ws.send_bytes(encode(s))
            else:
                ts = datetime.fromtimestamp(s.ts_ms / 1000, timezone.utc)
                out = []
                if not math.isnan(s.temp_c):
                    out.append(
                        b.body_temperature(s.temp_obs_id, s.patient_id, "stream", ts, s.temp_c)
                    )
                if not math.isnan(s.systolic):
                    out.append(
                        b.blood_pressure(
                            s.bp_obs_id, s.patient_id, "stream", ts, s.systolic, s.diastolic
                        )
                    )
                await ws.send_text(
                    json.dumps({"type": "observations", "alerts": s.alerts, "resources": out})
                )

    async def drain_client():
        while True:  # we ignore client messages but must read to detect disconnects
            await ws.receive_text()

    tasks = [asyncio.create_task(pump()), asyncio.create_task(drain_client())]
    try:
        await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
    finally:
        for t in tasks:
            t.cancel()
        svc.hub.unsubscribe(sub)
        log.info("ws_closed", user=principal.username, dropped=sub.dropped)
