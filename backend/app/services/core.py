"""Application services container + the ingestion pipeline shared by the simulator
and the REST `POST /fhir/Observation` endpoint."""

from __future__ import annotations

import asyncio
import math
import random
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from ..config import Env, Settings
from ..errors import ValidationFailed
from ..fhir import builders as b
from ..fhir import codes as c
from ..fhir.validation import validate_observation
from ..logging_setup import get_logger
from ..security.auth import Principal, UserStore
from ..security.crypto import Cipher, generate_key, load_or_create_dev_key
from ..security.middleware import RateLimiter
from .anomaly import AlertEngine
from .resilience import CircuitBreaker, supervise
from .simulator import DEMO_PATIENTS, VitalsSimulator
from .store import ResilientStore, SqliteStore
from .stream import Hub, Sample

log = get_logger("core")


def normalize_dt(value: str) -> str:
    dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return b.iso(dt)


class Services:
    def __init__(self, settings: Settings):
        self.settings = settings
        keys = settings.encryption_key_list
        if not keys:
            if settings.app_env is Env.prod:
                raise RuntimeError("ENCRYPTION_KEYS required")
            keys = [
                load_or_create_dev_key(Path(settings.database_path).with_suffix(".key"))
                if settings.database_path != ":memory:"
                else generate_key()
            ]
        self.cipher = Cipher(keys)
        self.db = SqliteStore(settings.database_path, self.cipher)
        self.store = ResilientStore(
            self.db,
            attempts=settings.store_retry_attempts,
            breaker=CircuitBreaker(
                "storage", settings.breaker_failure_threshold, settings.breaker_reset_timeout_s
            ),
            buffer_size=settings.fallback_buffer_size,
        )
        self.hub = Hub()
        self.alerts = AlertEngine()
        self.users = UserStore(settings)
        self.login_limiter = RateLimiter(settings.login_rate_per_minute)
        self.patients = list(DEMO_PATIENTS)
        self.patient_ids = [p.patient_id for p in self.patients]
        self._seq: dict[str, int] = {}
        self._tasks: list[asyncio.Task] = []
        self.started_at = datetime.now(timezone.utc)
        self._seed_reference_data()

    # ------------------------------------------------------------ lifecycle
    def _seed_reference_data(self) -> None:
        for p in self.patients:
            self.db.put(b.patient(p.patient_id, p.pseudonym, p.gender, p.birth_year))
            self.db.put(
                b.device(p.device_id, p.patient_id, "VitalPatch BT-2 (simulated)"),
                patient_id=p.patient_id,
            )

    async def start(self) -> None:
        self._tasks.append(
            asyncio.create_task(
                supervise("store-drain", self.store.drain_forever), name="store-drain"
            )
        )
        if self.settings.simulator_enabled:
            self._tasks.append(
                asyncio.create_task(supervise("simulator", self._run_simulator), name="simulator")
            )

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        await self.store.drain()
        self.db.close()

    # ------------------------------------------------------------ pipeline
    async def ingest_sample(
        self,
        patient_id: str,
        ts: datetime,
        temp_c: float | None,
        systolic: float | None,
        diastolic: float | None,
        device_id: str,
    ) -> list[dict]:
        """Build FHIR resources, validate, persist (resiliently) and publish to stream."""
        idx = self.patient_ids.index(patient_id)
        seq = self._seq[patient_id] = self._seq.get(patient_id, 0) + 1
        eff = b.iso(ts)
        resources = []
        if temp_c is not None:
            resources.append(
                b.body_temperature(f"{patient_id}-t-{seq}", patient_id, device_id, ts, temp_c)
            )
        if systolic is not None and diastolic is not None:
            resources.append(
                b.blood_pressure(
                    f"{patient_id}-bp-{seq}", patient_id, device_id, ts, systolic, diastolic
                )
            )
        for r in resources:
            issues = validate_observation(r, set(self.patient_ids))
            if issues:
                log.error("emitted_resource_invalid", id=r["id"], issues=len(issues))
                raise ValidationFailed("Generated resource failed validation", issues)
        for r in resources:
            await self.store.save(r, patient_id, r["code"]["coding"][0]["code"], eff)

        nan = float("nan")
        t = temp_c if temp_c is not None else nan
        s = systolic if systolic is not None else nan
        d = diastolic if diastolic is not None else nan
        flags = 0
        if not any(math.isnan(v) for v in (t, s, d)):
            flags = self.alerts.evaluate(patient_id, t, s, d)
        self.hub.publish(Sample(patient_id, idx, seq, ts.timestamp() * 1000, t, s, d, flags))
        return resources

    async def ingest_observation(self, data: dict[str, Any], principal: Principal) -> dict:
        issues = validate_observation(data, set(self.patient_ids))
        if issues:
            raise ValidationFailed("Observation failed validation", issues)
        pid = data["subject"]["reference"].split("/", 1)[1]
        ts = datetime.fromisoformat(normalize_dt(data["effectiveDateTime"]).replace("Z", "+00:00"))
        code = next(
            cd["code"]
            for cd in data["code"]["coding"]
            if cd.get("system") == c.LOINC and cd.get("code") in (c.CODE_BODY_TEMP, c.CODE_BP_PANEL)
        )
        dev = (data.get("device") or {}).get("reference", "Device/unknown").split("/")[-1]
        if code == c.CODE_BODY_TEMP:
            out = await self.ingest_sample(pid, ts, data["valueQuantity"]["value"], None, None, dev)
        else:
            comp = {
                cd["code"]: cp["valueQuantity"]["value"]
                for cp in data["component"]
                for cd in cp["code"]["coding"]
            }
            out = await self.ingest_sample(
                pid, ts, None, comp[c.CODE_SYSTOLIC], comp[c.CODE_DIASTOLIC], dev
            )
        return out[0]

    async def _run_simulator(self) -> None:
        seed = self.settings.simulator_seed
        sims = [
            VitalsSimulator(
                p, self.settings.simulator_hz, random.Random(None if seed is None else seed + i)
            )
            for i, p in enumerate(self.patients)
        ]
        period = 1.0 / self.settings.simulator_hz
        loop = asyncio.get_running_loop()
        next_tick = loop.time()
        log.info("simulator_started", patients=len(sims), hz=self.settings.simulator_hz)
        while True:
            ts = datetime.now(timezone.utc)
            for sim in sims:
                _, t, s, d = sim.step()
                await self.ingest_sample(sim.profile.patient_id, ts, t, s, d, sim.profile.device_id)
            next_tick += period
            await asyncio.sleep(max(0.0, next_tick - loop.time()))

    # ------------------------------------------------------------ audit
    def audit(
        self,
        *,
        action: str,
        ok: bool,
        principal: Principal | None,
        client_ip: str,
        entity_ref: str | None = None,
        query: str | None = None,
        subtype: str = "read",
    ) -> None:
        ev = b.audit_event(
            action=action,
            outcome_ok=ok,
            user=principal.username if principal else "anonymous",
            roles=principal.role_names if principal else [],
            client_ip=client_ip,
            entity_ref=entity_ref,
            query=query,
            subtype=subtype,
        )
        try:
            self.db.put_audit(ev)
        except Exception:  # noqa: BLE001 - audit failure must be visible but not fatal
            log.exception("audit_write_failed")
        log.info(
            "audit",
            action=action,
            outcome="success" if ok else "failure",
            entity=entity_ref,
            subtype=subtype,
        )
