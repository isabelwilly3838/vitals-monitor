"""Real-time fan-out of vitals samples.

Transport: WebSocket. Default encoding is a compact **binary frame** (28 bytes vs ~1.6 kB
for the two equivalent FHIR Observations); clients may request `fhir` encoding to receive
FHIR JSON Observations instead.

Binary frame layout (little-endian, 28 bytes):
    offset size type     field
    0      1    uint8    version (=1)
    1      1    uint8    patient index (into the patient list sent in the hello message)
    2      4    uint32   sequence number (per patient)
    6      8    float64  timestamp, ms since Unix epoch
    14     4    float32  body temperature, Cel
    18     4    float32  systolic, mm[Hg]
    22     4    float32  diastolic, mm[Hg]
    26     1    uint8    alert flags (bit0 fever, bit1 hypothermia, bit2 hypertension,
                                      bit3 hypotension, bit4 rapid change)
    27     1    uint8    reserved
"""

from __future__ import annotations

import asyncio
import struct
from dataclasses import dataclass

FRAME = struct.Struct("<BBIdfffBB")
FRAME_VERSION = 1
if FRAME.size != 28:  # pragma: no cover - layout guard
    raise RuntimeError("frame layout changed")

ALERT_FEVER = 1
ALERT_HYPOTHERMIA = 2
ALERT_HYPERTENSION = 4
ALERT_HYPOTENSION = 8
ALERT_RAPID_CHANGE = 16


@dataclass(frozen=True, slots=True)
class Sample:
    patient_id: str
    patient_index: int
    seq: int
    ts_ms: float
    temp_c: float
    systolic: float
    diastolic: float
    alerts: int = 0

    @property
    def temp_obs_id(self) -> str:
        return f"{self.patient_id}-t-{self.seq}"

    @property
    def bp_obs_id(self) -> str:
        return f"{self.patient_id}-bp-{self.seq}"


def encode(s: Sample) -> bytes:
    return FRAME.pack(
        FRAME_VERSION,
        s.patient_index,
        s.seq & 0xFFFFFFFF,
        s.ts_ms,
        s.temp_c,
        s.systolic,
        s.diastolic,
        s.alerts & 0xFF,
        0,
    )


def decode(buf: bytes, patient_ids: list[str]) -> Sample:
    if len(buf) != FRAME.size:
        raise ValueError(f"frame must be {FRAME.size} bytes")
    ver, pidx, seq, ts, t, sy, di, al, _ = FRAME.unpack(buf)
    if ver != FRAME_VERSION:
        raise ValueError("unsupported frame version")
    if pidx >= len(patient_ids):
        raise ValueError("unknown patient index")
    return Sample(patient_ids[pidx], pidx, seq, ts, t, sy, di, al)


class Subscriber:
    """Bounded per-client queue: a slow client drops its *oldest* frames instead of
    blocking the producer or growing memory (back-pressure isolation)."""

    def __init__(self, patient_filter: set[str] | None, maxsize: int = 256):
        self.queue: asyncio.Queue[Sample] = asyncio.Queue(maxsize=maxsize)
        self.patient_filter = patient_filter
        self.dropped = 0

    def offer(self, s: Sample) -> None:
        if self.patient_filter is not None and s.patient_id not in self.patient_filter:
            return
        if self.queue.full():
            try:
                self.queue.get_nowait()
                self.dropped += 1
            except asyncio.QueueEmpty:
                pass
        self.queue.put_nowait(s)


class Hub:
    def __init__(self) -> None:
        self._subs: set[Subscriber] = set()
        self.published = 0

    def subscribe(self, patient_filter: set[str] | None) -> Subscriber:
        sub = Subscriber(patient_filter)
        self._subs.add(sub)
        return sub

    def unsubscribe(self, sub: Subscriber) -> None:
        self._subs.discard(sub)

    def publish(self, s: Sample) -> None:
        self.published += 1
        for sub in list(self._subs):
            sub.offer(s)

    @property
    def subscriber_count(self) -> int:
        return len(self._subs)
