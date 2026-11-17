"""Wearable sensor simulator: body temperature + blood pressure per patient.

Models a per-patient baseline, a slow circadian drift, measurement noise, mean-reverting
random walk, and occasional clinical episodes (fever onset, hypertensive spike) so the
dashboard has realistic events to show.
"""

from __future__ import annotations

import math
import random
from dataclasses import dataclass, field


@dataclass
class PatientProfile:
    patient_id: str
    pseudonym: str
    gender: str
    birth_year: int
    device_id: str
    base_temp: float
    base_sys: float
    base_dia: float


DEMO_PATIENTS = [
    PatientProfile("P001", "STUDY-0001", "female", 1958, "D001", 36.7, 128, 82),
    PatientProfile("P002", "STUDY-0002", "male", 1971, "D002", 36.9, 131, 84),
    PatientProfile("P003", "STUDY-0003", "female", 1994, "D003", 36.5, 112, 72),
]


@dataclass
class _State:
    temp: float
    sys: float
    dia: float
    episode: str | None = None
    episode_left: int = 0
    seq: int = 0
    t: float = 0.0


@dataclass
class VitalsSimulator:
    profile: PatientProfile
    hz: float = 1.0
    rng: random.Random = field(default_factory=random.Random)

    def __post_init__(self) -> None:
        p = self.profile
        self.state = _State(p.base_temp, p.base_sys, p.base_dia)

    def step(self) -> tuple[int, float, float, float]:
        """Advance one tick; returns (seq, temp_c, systolic, diastolic)."""
        s, p, r = self.state, self.profile, self.rng
        dt = 1.0 / self.hz
        s.t += dt
        s.seq += 1

        if s.episode is None and r.random() < 0.004 * dt:
            s.episode = r.choice(["fever", "hypertension", "hypotension"])
            s.episode_left = int(r.uniform(40, 120) * self.hz)
        target_t, target_s, target_d = p.base_temp, p.base_sys, p.base_dia
        if s.episode == "fever":
            target_t += 1.9
        elif s.episode == "hypertension":
            target_s += 32
            target_d += 14
        elif s.episode == "hypotension":
            target_s -= 30
            target_d -= 18
        if s.episode:
            s.episode_left -= 1
            if s.episode_left <= 0:
                s.episode = None

        circadian = 0.25 * math.sin(2 * math.pi * s.t / 600)  # compressed "day" (10 min)
        k_t, k_bp = 0.03 * dt, 0.08 * dt
        s.temp += k_t * (target_t + circadian - s.temp) + r.gauss(0, 0.015)
        s.sys += k_bp * (target_s - s.sys) + r.gauss(0, 0.9)
        s.dia += k_bp * (target_d - s.dia) + r.gauss(0, 0.6)

        temp = round(s.temp + r.gauss(0, 0.03), 2)
        sys_ = round(max(s.sys + r.gauss(0, 1.5), 60.0), 1)
        dia = round(min(max(s.dia + r.gauss(0, 1.0), 35.0), sys_ - 15), 1)
        return s.seq, temp, sys_, dia
