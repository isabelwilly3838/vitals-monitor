"""Environment-aware configuration.

Loading order (later wins):
  1. defaults in this class
  2. config/<APP_ENV>.env          (non-secret, committed per-environment config)
  3. .env                          (local overrides, git-ignored)
  4. real environment variables
  5. <NAME>_FILE variables         (Docker/K8s secrets mounted as files)

Settings are validated at startup; production refuses to boot with weak/default secrets.
"""

from __future__ import annotations

import os
from enum import StrEnum
from functools import lru_cache
from pathlib import Path

from pydantic import Field, field_validator, model_validator
from pydantic_settings import BaseSettings, SettingsConfigDict

ROOT_DIR = Path(__file__).resolve().parents[2]
CONFIG_DIR = ROOT_DIR / "config"

DEV_PLACEHOLDER = "dev-only-insecure"


class Env(StrEnum):
    dev = "dev"
    test = "test"
    prod = "prod"


def _env_files() -> tuple[Path, ...]:
    env = os.getenv("APP_ENV", "dev")
    return (CONFIG_DIR / f"{env}.env", ROOT_DIR / ".env")


def _read_secret_files() -> None:
    """Support FOO_FILE=/run/secrets/foo for every setting (Docker secrets)."""
    for key, value in list(os.environ.items()):
        if key.endswith("_FILE") and key[:-5] not in os.environ:
            path = Path(value)
            if path.is_file():
                os.environ[key[:-5]] = path.read_text().strip()


class Settings(BaseSettings):
    model_config = SettingsConfigDict(extra="ignore", env_file_encoding="utf-8")

    app_env: Env = Env.dev
    app_name: str = "Vitals Monitor"
    log_level: str = "INFO"
    log_json: bool = True

    # --- security ---
    # Comma-separated. First entry signs, all entries verify (zero-downtime rotation).
    jwt_secrets: str = f"{DEV_PLACEHOLDER}-jwt-secret-change-me-please-000000"
    jwt_algorithm: str = "HS256"
    jwt_ttl_minutes: int = Field(15, ge=1, le=240)
    jwt_issuer: str = "vitals-monitor"
    # Comma-separated Fernet keys. First entry encrypts, all entries decrypt.
    encryption_keys: str = ""
    # JSON list of users: [{"username","password_hash","roles":[..],"patient_id":?}]
    users_json: str = ""
    cors_origins: str = ""
    login_rate_per_minute: int = 10
    max_body_bytes: int = 64 * 1024

    # --- storage ---
    database_path: str = str(ROOT_DIR / "data" / "vitals.db")

    # --- simulator / stream ---
    simulator_enabled: bool = True
    simulator_hz: float = Field(1.0, gt=0, le=50)
    simulator_seed: int | None = None
    ws_auth_timeout_s: float = 5.0

    # --- resilience ---
    store_retry_attempts: int = 3
    breaker_failure_threshold: int = 5
    breaker_reset_timeout_s: float = 10.0
    fallback_buffer_size: int = 10_000

    serve_frontend: bool = True
    docs_enabled: bool = True

    @field_validator("database_path")
    @classmethod
    def _abs_db(cls, v: str) -> str:
        if v == ":memory:" or Path(v).is_absolute():
            return v
        return str(ROOT_DIR / v)

    @field_validator("log_level")
    @classmethod
    def _level(cls, v: str) -> str:
        v = v.upper()
        if v not in {"DEBUG", "INFO", "WARNING", "ERROR"}:
            raise ValueError("invalid log level")
        return v

    @property
    def jwt_secret_list(self) -> list[str]:
        return [s.strip() for s in self.jwt_secrets.split(",") if s.strip()]

    @property
    def encryption_key_list(self) -> list[str]:
        return [k.strip() for k in self.encryption_keys.split(",") if k.strip()]

    @property
    def cors_origin_list(self) -> list[str]:
        return [o.strip() for o in self.cors_origins.split(",") if o.strip()]

    @model_validator(mode="after")
    def _prod_hardening(self) -> Settings:
        if self.app_env is not Env.prod:
            return self
        problems = []
        if any(DEV_PLACEHOLDER in s or len(s) < 32 for s in self.jwt_secret_list):
            problems.append("JWT_SECRETS must be set, >=32 chars and not the dev default")
        if not self.encryption_key_list:
            problems.append("ENCRYPTION_KEYS must be set")
        if "*" in self.cors_origin_list:
            problems.append("CORS_ORIGINS may not be '*' in prod")
        if not self.users_json:
            problems.append("USERS_JSON must be provided in prod (no seeded dev users)")
        if self.docs_enabled:
            problems.append("DOCS_ENABLED must be false in prod")
        if problems:
            raise ValueError("Unsafe production config: " + "; ".join(problems))
        return self


@lru_cache
def get_settings() -> Settings:
    _read_secret_files()
    return Settings(_env_file=_env_files())
