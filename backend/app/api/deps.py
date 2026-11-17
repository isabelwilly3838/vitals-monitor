from __future__ import annotations

from fastapi import Request

from ..services.core import Services


def services(request: Request) -> Services:
    return request.app.state.services


def client_ip(request: Request) -> str:
    # Behind nginx the real client IP arrives in X-Real-IP (set by our proxy only).
    return request.headers.get("x-real-ip") or (request.client.host if request.client else "-")
