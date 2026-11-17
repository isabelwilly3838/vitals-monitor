"""Fault-tolerance primitives: retry with exponential backoff + jitter, a circuit breaker,
and a supervisor that restarts crashed background tasks (self-healing)."""

from __future__ import annotations

import asyncio
import random
import time
from collections.abc import Awaitable, Callable
from enum import StrEnum
from typing import TypeVar

from ..logging_setup import get_logger

log = get_logger("resilience")
T = TypeVar("T")


async def retry_async(
    fn: Callable[[], Awaitable[T]],
    *,
    attempts: int = 3,
    base_delay: float = 0.05,
    max_delay: float = 2.0,
    retry_on: tuple[type[BaseException], ...] = (Exception,),
    name: str = "op",
) -> T:
    for attempt in range(1, attempts + 1):
        try:
            return await fn()
        except retry_on as e:
            if attempt == attempts:
                log.warning("retry_exhausted", op=name, attempts=attempts, error=type(e).__name__)
                raise
            delay = min(max_delay, base_delay * 2 ** (attempt - 1)) * (0.5 + random.random())
            log.info(
                "retrying",
                op=name,
                attempt=attempt,
                delay_s=round(delay, 3),
                error=type(e).__name__,
            )
            await asyncio.sleep(delay)
    raise RuntimeError("unreachable")


class BreakerState(StrEnum):
    closed = "closed"
    open = "open"
    half_open = "half_open"


class CircuitOpenError(Exception):
    pass


class CircuitBreaker:
    """closed --(N consecutive failures)--> open --(timeout)--> half_open --ok--> closed
    \\--fail--> open"""

    def __init__(
        self,
        name: str,
        failure_threshold: int = 5,
        reset_timeout: float = 10.0,
        clock: Callable[[], float] = time.monotonic,
    ):
        self.name = name
        self.failure_threshold = failure_threshold
        self.reset_timeout = reset_timeout
        self._clock = clock
        self._state = BreakerState.closed
        self._failures = 0
        self._opened_at = 0.0
        self._half_open_inflight = False

    @property
    def state(self) -> BreakerState:
        if (
            self._state is BreakerState.open
            and self._clock() - self._opened_at >= self.reset_timeout
        ):
            self._state = BreakerState.half_open
            self._half_open_inflight = False
        return self._state

    async def call(self, fn: Callable[[], Awaitable[T]]) -> T:
        state = self.state
        if state is BreakerState.open:
            raise CircuitOpenError(self.name)
        if state is BreakerState.half_open:
            if self._half_open_inflight:
                raise CircuitOpenError(self.name)
            self._half_open_inflight = True
        try:
            result = await fn()
        except Exception:
            self._on_failure()
            raise
        self._on_success()
        return result

    def _on_success(self) -> None:
        if self._state is not BreakerState.closed:
            log.info("breaker_closed", breaker=self.name)
        self._state = BreakerState.closed
        self._failures = 0
        self._half_open_inflight = False

    def _on_failure(self) -> None:
        self._failures += 1
        if self._state is BreakerState.half_open or self._failures >= self.failure_threshold:
            if self._state is not BreakerState.open:
                log.error("breaker_opened", breaker=self.name, failures=self._failures)
            self._state = BreakerState.open
            self._opened_at = self._clock()
            self._half_open_inflight = False


async def supervise(
    name: str, factory: Callable[[], Awaitable[None]], *, max_backoff: float = 30.0
) -> None:
    """Run `factory()` forever; restart with exponential backoff if it crashes."""
    backoff = 0.5
    while True:
        started = time.monotonic()
        try:
            await factory()
            return
        except asyncio.CancelledError:
            raise
        except Exception:
            log.exception("task_crashed_restarting", task=name, backoff_s=backoff)
            await asyncio.sleep(backoff)
            backoff = 0.5 if time.monotonic() - started > 60 else min(max_backoff, backoff * 2)
