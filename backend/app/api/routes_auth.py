from __future__ import annotations

from fastapi import APIRouter, Depends, Request
from fastapi.security import OAuth2PasswordRequestForm

from ..config import Settings, get_settings
from ..errors import AuthError, RateLimited
from ..security.auth import Principal, current_principal, issue_token
from ..services.core import Services
from .deps import client_ip, services

router = APIRouter(prefix="/auth", tags=["auth"])


@router.post("/token")
async def login(
    request: Request,
    form: OAuth2PasswordRequestForm = Depends(),
    svc: Services = Depends(services),
    settings: Settings = Depends(get_settings),
):
    ip = client_ip(request)
    if not svc.login_limiter.allow(ip):
        raise RateLimited("Too many login attempts, try again later")
    if len(form.username) > 64 or len(form.password) > 128:
        raise AuthError("Invalid credentials")
    principal = svc.users.authenticate(form.username, form.password)
    if principal is None:
        svc.audit(
            action="login",
            ok=False,
            principal=Principal(form.username[:64], []),
            client_ip=ip,
            subtype="login",
        )
        raise AuthError("Invalid credentials")
    token, ttl = issue_token(principal, settings)
    svc.audit(action="login", ok=True, principal=principal, client_ip=ip, subtype="login")
    return {
        "access_token": token,
        "token_type": "bearer",
        "expires_in": ttl,
        "roles": principal.role_names,
        "patient_id": principal.patient_id,
    }


@router.get("/me")
async def me(p: Principal = Depends(current_principal)):
    return {"username": p.username, "roles": p.role_names, "patient_id": p.patient_id}
