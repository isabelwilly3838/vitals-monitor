"""Real-time clinical alerting: threshold rules + rate-of-change detection (rolling
z-score over a short window). Produces bit flags carried in every stream frame."""

from __future__ import annotations

from collections import deque
from statistics import fmean, pstdev

from ..fhir import codes as c
from ..logging_setup import get_logger
from .stream import (
    ALERT_FEVER,
    ALERT_HYPERTENSION,
    ALERT_HYPOTENSION,
    ALERT_HYPOTHERMIA,
    ALERT_RAPID_CHANGE,
)

log = get_logger("alerts")


def threshold_flags(temp_c: float, systolic: float, diastolic: float) -> int:
    f = 0
    if temp_c >= c.TEMP_HIGH:
        f |= ALERT_FEVER
    if temp_c < c.TEMP_LOW:
        f |= ALERT_HYPOTHERMIA
    if systolic >= c.SYS_HIGH or diastolic >= c.DIA_HIGH:
        f |= ALERT_HYPERTENSION
    if systolic < c.SYS_LOW or diastolic < c.DIA_LOW:
        f |= ALERT_HYPOTENSION
    return f


class RateOfChangeDetector:
    """Flags a sample whose systolic deviates > z_limit std-devs from the recent window."""

    def __init__(self, window: int = 30, z_limit: float = 4.0, min_std: float = 1.5):
        self.window: deque[float] = deque(maxlen=window)
        self.z_limit = z_limit
        self.min_std = min_std

    def check(self, systolic: float) -> bool:
        flag = False
        if len(self.window) >= 10:
            mu = fmean(self.window)
            sd = max(pstdev(self.window), self.min_std)
            flag = abs(systolic - mu) / sd > self.z_limit
        self.window.append(systolic)
        return flag


class AlertEngine:
    def __init__(self) -> None:
        self._roc: dict[str, RateOfChangeDetector] = {}
        self._last: dict[str, int] = {}

    def evaluate(self, patient_id: str, temp_c: float, systolic: float, diastolic: float) -> int:
        flags = threshold_flags(temp_c, systolic, diastolic)
        if self._roc.setdefault(patient_id, RateOfChangeDetector()).check(systolic):
            flags |= ALERT_RAPID_CHANGE
        # log only on transitions to avoid flooding the log pipeline
        if flags != self._last.get(patient_id, 0):
            if flags:
                log.warning("vital_alert_raised", patient=patient_id, flags=flags)
            else:
                log.info("vital_alert_cleared", patient=patient_id)
            self._last[patient_id] = flags
        return flags
