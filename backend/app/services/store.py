"""Encrypted FHIR resource store (SQLite) with a resilient wrapper.

Only non-sensitive index columns (type, patient id, LOINC code, effective time) are stored
in clear text so searches work; the full FHIR JSON is encrypted with Fernet.
All SQL is parameterised (no string interpolation of user input -> no SQL injection).
"""

from __future__ import annotations

import asyncio
import json
import sqlite3
import threading
from collections import deque
from pathlib import Path
from typing import Any

from ..logging_setup import get_logger
from ..security.crypto import Cipher
from .resilience import CircuitBreaker, CircuitOpenError, retry_async

log = get_logger("store")

SCHEMA = """
CREATE TABLE IF NOT EXISTS resources (
    rtype      TEXT NOT NULL,
    id         TEXT NOT NULL,
    patient_id TEXT,
    code       TEXT,
    effective  TEXT,
    data       BLOB NOT NULL,
    PRIMARY KEY (rtype, id)
);
CREATE INDEX IF NOT EXISTS ix_obs ON resources (rtype, patient_id, code, effective);
CREATE TABLE IF NOT EXISTS audit (
    id       TEXT PRIMARY KEY,
    recorded TEXT NOT NULL,
    username TEXT NOT NULL,
    action   TEXT NOT NULL,
    data     BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_audit ON audit (recorded);
"""


class StorageError(Exception):
    pass


class SqliteStore:
    def __init__(self, path: str, cipher: Cipher):
        self.cipher = cipher
        if path != ":memory:":
            Path(path).parent.mkdir(parents=True, exist_ok=True)
        self._conn = sqlite3.connect(path, check_same_thread=False, isolation_level=None)
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute("PRAGMA synchronous=NORMAL")
        self._conn.executescript(SCHEMA)
        self._lock = threading.Lock()
        self.fail_mode = False  # chaos switch for demonstrating the circuit breaker

    def _check(self) -> None:
        if self.fail_mode:
            raise StorageError("simulated storage outage")

    def _enc(self, obj: dict) -> bytes:
        return self.cipher.encrypt(json.dumps(obj, separators=(",", ":")).encode())

    def _dec(self, blob: bytes) -> dict:
        return json.loads(self.cipher.decrypt(blob))

    # -------- writes
    def put(
        self,
        resource: dict[str, Any],
        patient_id: str | None = None,
        code: str | None = None,
        effective: str | None = None,
    ) -> None:
        self._check()
        with self._lock:
            self._conn.execute(
                "INSERT OR REPLACE INTO resources (rtype,id,patient_id,code,effective,data) "
                "VALUES (?,?,?,?,?,?)",
                (
                    resource["resourceType"],
                    resource["id"],
                    patient_id,
                    code,
                    effective,
                    self._enc(resource),
                ),
            )

    def put_many(self, rows: list[tuple[dict, str | None, str | None, str | None]]) -> None:
        self._check()
        with self._lock:
            self._conn.execute("BEGIN")
            try:
                self._conn.executemany(
                    "INSERT OR REPLACE INTO resources (rtype,id,patient_id,code,effective,data) "
                    "VALUES (?,?,?,?,?,?)",
                    [(r["resourceType"], r["id"], p, c, e, self._enc(r)) for r, p, c, e in rows],
                )
                self._conn.execute("COMMIT")
            except Exception:
                self._conn.execute("ROLLBACK")
                raise

    def put_audit(self, event: dict) -> None:
        with self._lock:
            who = event["agent"][0]["who"]["identifier"]["value"]
            self._conn.execute(
                "INSERT INTO audit (id,recorded,username,action,data) VALUES (?,?,?,?,?)",
                (event["id"], event["recorded"], who, event["action"], self._enc(event)),
            )

    # -------- reads
    def get(self, rtype: str, rid: str) -> dict | None:
        self._check()
        row = self._conn.execute(
            "SELECT data FROM resources WHERE rtype=? AND id=?", (rtype, rid)
        ).fetchone()
        return self._dec(row[0]) if row else None

    def list_type(self, rtype: str) -> list[dict]:
        self._check()
        rows = self._conn.execute(
            "SELECT data FROM resources WHERE rtype=? ORDER BY id", (rtype,)
        ).fetchall()
        return [self._dec(r[0]) for r in rows]

    def search_observations(
        self,
        *,
        patient_id: str | None,
        code: str | None,
        date_ge: str | None,
        date_le: str | None,
        limit: int,
    ) -> tuple[list[dict], int]:
        self._check()
        where, args = ["rtype='Observation'"], []
        if patient_id:
            where.append("patient_id=?")
            args.append(patient_id)
        if code:
            where.append("code=?")
            args.append(code)
        if date_ge:
            where.append("effective>=?")
            args.append(date_ge)
        if date_le:
            where.append("effective<=?")
            args.append(date_le)
        clause = " AND ".join(where)
        total = self._conn.execute(
            f"SELECT COUNT(*) FROM resources WHERE {clause}",  # noqa: S608
            args,
        ).fetchone()[0]
        rows = self._conn.execute(
            f"SELECT data FROM resources WHERE {clause} ORDER BY effective DESC LIMIT ?",  # noqa: S608
            [*args, limit],
        ).fetchall()
        return [self._dec(r[0]) for r in rows], total

    def list_audit(self, limit: int) -> list[dict]:
        rows = self._conn.execute(
            "SELECT data FROM audit ORDER BY recorded DESC LIMIT ?", (limit,)
        ).fetchall()
        return [self._dec(r[0]) for r in rows]

    def rotate_keys(self) -> int:
        """Re-encrypt every row with the current primary key."""
        n = 0
        with self._lock:
            self._conn.execute("BEGIN")
            try:
                for table, keycols in (("resources", ("rtype", "id")), ("audit", ("id",))):
                    rows = self._conn.execute(
                        f"SELECT {','.join(keycols)}, data FROM {table}"  # noqa: S608
                    ).fetchall()
                    for row in rows:
                        new = self.cipher.rotate(row[-1])
                        cond = " AND ".join(f"{k}=?" for k in keycols)
                        self._conn.execute(
                            f"UPDATE {table} SET data=? WHERE {cond}",  # noqa: S608
                            (new, *row[:-1]),
                        )
                        n += 1
                self._conn.execute("COMMIT")
            except Exception:
                self._conn.execute("ROLLBACK")
                raise
        return n

    def ping(self) -> None:
        self._check()
        self._conn.execute("SELECT 1").fetchone()

    def close(self) -> None:
        self._conn.close()


class ResilientStore:
    """Wraps writes in retry + circuit breaker; on outage, buffers writes in memory
    (bounded) and drains them once storage recovers -> graceful degradation, no data loss
    for short outages, live stream keeps running."""

    def __init__(
        self, store: SqliteStore, *, attempts: int, breaker: CircuitBreaker, buffer_size: int
    ):
        self.store = store
        self.attempts = attempts
        self.breaker = breaker
        self.buffer: deque[tuple[dict, str | None, str | None, str | None]] = deque(
            maxlen=buffer_size
        )
        self.dropped = 0

    async def save(
        self, resource: dict, patient_id: str | None, code: str | None, effective: str | None
    ) -> bool:
        row = (resource, patient_id, code, effective)
        try:
            await self.breaker.call(
                lambda: retry_async(
                    lambda: asyncio.to_thread(self.store.put, *row),
                    attempts=self.attempts,
                    name="store.put",
                )
            )
            return True
        except (CircuitOpenError, Exception) as e:  # noqa: BLE001
            if len(self.buffer) == self.buffer.maxlen:
                self.dropped += 1
            self.buffer.append(row)
            log.warning(
                "store_degraded_buffering", buffered=len(self.buffer), reason=type(e).__name__
            )
            return False

    async def drain(self) -> int:
        if not self.buffer:
            return 0
        batch = list(self.buffer)
        try:
            await self.breaker.call(lambda: asyncio.to_thread(self.store.put_many, batch))
        except Exception:  # noqa: BLE001
            return 0
        for _ in batch:
            self.buffer.popleft()
        log.info("store_buffer_drained", count=len(batch))
        return len(batch)

    async def drain_forever(self, interval: float = 2.0) -> None:
        while True:
            await asyncio.sleep(interval)
            await self.drain()

    def status(self) -> dict[str, Any]:
        return {
            "breaker": self.breaker.state.value,
            "buffered": len(self.buffer),
            "dropped": self.dropped,
        }
