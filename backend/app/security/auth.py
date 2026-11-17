"""Token authentication + role-based access control.

Roles
  admin      everything, incl. audit log and key rotation
  clinician  read all patients, ingest observations
  patient    read *own* data only (token carries patient_id)
  device     ingest observations only (sensor gateway)
"""

from __future__ import annotations

import json
import time
import uuid
from dataclasses import dataclass, field
from enum import StrEnum

import bcrypt
import jwt
from fastapi import Depends, Request
from fastapi.security import OAuth2PasswordBearer

from ..config import Env, Settings, get_settings
from ..errors import AuthError, ForbiddenError


class Role(StrEnum):
    admin = "admin"
    clinician = "clinician"
    patient = "patient"
    device = "device"


class Perm(StrEnum):
    read_patient = "patient:read"
    read_obs = "observation:read"
    write_obs = "observation:write"
    read_audit = "audit:read"
    admin_keys = "keys:rotate"
    stream = "stream:subscribe"


ROLE_PERMS: dict[Role, set[Perm]] = {
    Role.admin: set(Perm),
    Role.clinician: {Perm.read_patient, Perm.read_obs, Perm.write_obs, Perm.stream},
    Role.patient: {Perm.read_patient, Perm.read_obs, Perm.stream},
    Role.device: {Perm.write_obs},
}


@dataclass
class Principal:
    username: str
    roles: list[Role]
    patient_id: str | None = None
    jti: str = field(default_factory=lambda: uuid.uuid4().hex)

    def has(self, perm: Perm) -> bool:
        return any(perm in ROLE_PERMS[r] for r in self.roles)

    def can_access_patient(self, patient_id: str) -> bool:
        if Role.admin in self.roles or Role.clinician in self.roles:
            return True
        return Role.patient in self.roles and self.patient_id == patient_id

    @property
    def role_names(self) -> list[str]:
        return [r.value for r in self.roles]


# ---------------------------------------------------------------- passwords / users


def hash_password(password: str) -> str:
    return bcrypt.hashpw(password.encode(), bcrypt.gensalt(rounds=12)).decode()


def verify_password(password: str, hashed: str) -> bool:
    try:
        return bcrypt.checkpw(password.encode(), hashed.encode())
    except ValueError:
        return False


# Development/test accounts. Never loaded when APP_ENV=prod (config validation enforces
# USERS_JSON there). Passwords are documented in README as local test credentials.
DEV_USERS = [
    ("admin", "admin-dev-pass", ["admin"], None),
    ("clinician", "clinician-dev-pass", ["clinician"], None),
    ("patient1", "patient-dev-pass", ["patient"], "P001"),
    ("gateway", "device-dev-pass", ["device"], None),
]


class UserStore:
    def __init__(self, settings: Settings):
        self._users: dict[str, dict] = {}
        if settings.users_json:
            for u in json.loads(settings.users_json):
                self._users[u["username"]] = u
        elif settings.app_env is not Env.prod:
            rounds = 4 if settings.app_env is Env.test else 12
            for name, pw, roles, pid in DEV_USERS:
                h = bcrypt.hashpw(pw.encode(), bcrypt.gensalt(rounds=rounds)).decode()
                self._users[name] = {
                    "username": name,
                    "password_hash": h,
                    "roles": roles,
                    "patient_id": pid,
                }
        # constant-time-ish failure path for unknown users
        self._dummy = bcrypt.hashpw(b"dummy", bcrypt.gensalt(rounds=4)).decode()

    def authenticate(self, username: str, password: str) -> Principal | None:
        u = self._users.get(username)
        if not u:
            verify_password(password, self._dummy)
            return None
        if not verify_password(password, u["password_hash"]):
            return None
        return Principal(
            username=username, roles=[Role(r) for r in u["roles"]], patient_id=u.get("patient_id")
        )


# ---------------------------------------------------------------- JWT


def issue_token(p: Principal, settings: Settings) -> tuple[str, int]:
    now = int(time.time())
    ttl = settings.jwt_ttl_minutes * 60
    claims = {
        "sub": p.username,
        "roles": p.role_names,
        "iat": now,
        "nbf": now,
        "exp": now + ttl,
        "iss": settings.jwt_issuer,
        "jti": p.jti,
    }
    if p.patient_id:
        claims["patient_id"] = p.patient_id
    # kid = index-free fingerprint so verifiers know which secret signed it
    token = jwt.encode(
        claims,
        settings.jwt_secret_list[0],
        algorithm=settings.jwt_algorithm,
        headers={"kid": _kid(settings.jwt_secret_list[0])},
    )
    return token, ttl


def _kid(secret: str) -> str:
    import hashlib

    return hashlib.sha256(secret.encode()).hexdigest()[:12]


def decode_token(token: str, settings: Settings) -> Principal:
    try:
        kid = jwt.get_unverified_header(token).get("kid")
    except jwt.PyJWTError as e:
        raise AuthError("Invalid token") from e
    secrets = [s for s in settings.jwt_secret_list if _kid(s) == kid] or settings.jwt_secret_list
    last_err: Exception | None = None
    for secret in secrets:
        try:
            claims = jwt.decode(
                token,
                secret,
                algorithms=[settings.jwt_algorithm],
                issuer=settings.jwt_issuer,
                options={"require": ["exp", "iat", "sub", "iss"]},
            )
            return Principal(
                username=claims["sub"],
                roles=[Role(r) for r in claims.get("roles", [])],
                patient_id=claims.get("patient_id"),
                jti=claims.get("jti", ""),
            )
        except jwt.ExpiredSignatureError as e:
            raise AuthError("Token expired") from e
        except (jwt.PyJWTError, ValueError) as e:
            last_err = e
    raise AuthError("Invalid token") from last_err


oauth2_scheme = OAuth2PasswordBearer(tokenUrl="/auth/token", auto_error=False)


def current_principal(
    request: Request,
    token: str | None = Depends(oauth2_scheme),
    settings: Settings = Depends(get_settings),
) -> Principal:
    if not token:
        raise AuthError("Missing bearer token")
    p = decode_token(token, settings)
    request.state.principal = p
    from ..logging_setup import user_var

    user_var.set(p.username)
    return p


def require(perm: Perm):
    def _dep(p: Principal = Depends(current_principal)) -> Principal:
        if not p.has(perm):
            raise ForbiddenError(f"Role lacks permission '{perm.value}'")
        return p

    return _dep
