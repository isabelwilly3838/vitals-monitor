"""HTTP middleware: request correlation, access logging, security headers, body limits,
and a small in-memory token-bucket rate limiter (login brute-force protection)."""

from __future__ import annotations

import re
import time
import uuid
from collections import defaultdict

from starlette.middleware.base import BaseHTTPMiddleware
from starlette.requests import Request
from starlette.types import ASGIApp, Message, Receive, Scope, Send

from ..errors import outcome_response
from ..logging_setup import get_logger, request_id_var, user_var

log = get_logger("http")
RID_RE = re.compile(r"^[A-Za-z0-9\-]{8,64}$")

SECURITY_HEADERS = {
    "X-Content-Type-Options": "nosniff",
    "X-Frame-Options": "DENY",
    "Referrer-Policy": "no-referrer",
    "Permissions-Policy": "camera=(), microphone=(), geolocation=()",
    "Cross-Origin-Opener-Policy": "same-origin",
    "Cache-Control": "no-store",
    "Content-Security-Policy": (
        "default-src 'self'; script-src 'self' https://cdn.jsdelivr.net; "
        "style-src 'self'; img-src 'self' data:; connect-src 'self' ws: wss:; "
        "frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
    ),
}


class RequestContextMiddleware(BaseHTTPMiddleware):
    async def dispatch(self, request: Request, call_next):
        incoming = request.headers.get("x-request-id", "")
        rid = incoming if RID_RE.match(incoming) else uuid.uuid4().hex
        request_id_var.set(rid)
        user_var.set(None)
        start = time.perf_counter()
        response = await call_next(request)
        dur = (time.perf_counter() - start) * 1000
        response.headers["X-Request-ID"] = rid
        for k, v in SECURITY_HEADERS.items():
            if k == "Cache-Control" and not request.url.path.startswith(
                ("/fhir", "/auth", "/admin")
            ):
                continue
            response.headers.setdefault(k, v)
        if request.url.path not in ("/health/live",):
            log.info(
                "http_request",
                method=request.method,
                path=request.url.path,
                status=response.status_code,
                duration_ms=round(dur, 2),
                client=request.client.host if request.client else None,
            )
        return response


class BodySizeLimitMiddleware:
    """Reject oversize bodies before they are buffered (pure ASGI, streams-aware)."""

    def __init__(self, app: ASGIApp, max_bytes: int):
        self.app = app
        self.max_bytes = max_bytes

    async def __call__(self, scope: Scope, receive: Receive, send: Send):
        if scope["type"] != "http":
            return await self.app(scope, receive, send)
        headers = dict(scope.get("headers", []))
        cl = headers.get(b"content-length")
        if cl and cl.isdigit() and int(cl) > self.max_bytes:
            resp = outcome_response(
                413,
                [
                    {
                        "severity": "error",
                        "code": "too-costly",
                        "diagnostics": "Request body too large",
                    }
                ],
            )
            return await resp(scope, receive, send)
        received = 0

        async def limited_receive() -> Message:
            nonlocal received
            msg = await receive()
            if msg["type"] == "http.request":
                received += len(msg.get("body", b""))
                if received > self.max_bytes:
                    raise _TooLarge()
            return msg

        try:
            await self.app(scope, limited_receive, send)
        except _TooLarge:
            resp = outcome_response(
                413,
                [
                    {
                        "severity": "error",
                        "code": "too-costly",
                        "diagnostics": "Request body too large",
                    }
                ],
            )
            await resp(scope, receive, send)


class _TooLarge(Exception):
    pass


class RateLimiter:
    """Token bucket per key. Single-process; swap for Redis when running >1 replica."""

    def __init__(self, per_minute: int):
        self.capacity = max(per_minute, 1)
        self.rate = self.capacity / 60.0
        self._buckets: dict[str, tuple[float, float]] = defaultdict(
            lambda: (float(self.capacity), time.monotonic())
        )

    def allow(self, key: str) -> bool:
        tokens, last = self._buckets[key]
        now = time.monotonic()
        tokens = min(self.capacity, tokens + (now - last) * self.rate)
        if tokens < 1:
            self._buckets[key] = (tokens, now)
            return False
        self._buckets[key] = (tokens - 1, now)
        return True
