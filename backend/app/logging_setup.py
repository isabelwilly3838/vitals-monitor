"""Structured JSON logging with request correlation and PHI redaction.

Every log line is one JSON object with an ISO-8601 UTC timestamp, level, logger, event,
request_id and (when authenticated) the acting user. Fields that could contain protected
health information are redacted before the line is written, so stdout can be shipped to a
central store (Loki / ELK) without leaking PHI.
"""

from __future__ import annotations

import logging
import sys
from contextvars import ContextVar
from typing import Any

import structlog

request_id_var: ContextVar[str | None] = ContextVar("request_id", default=None)
user_var: ContextVar[str | None] = ContextVar("user", default=None)

# Keys that must never reach the log sink in clear text.
PHI_KEYS = {
    "name",
    "given",
    "family",
    "birthdate",
    "birth_date",
    "address",
    "telecom",
    "phone",
    "email",
    "password",
    "password_hash",
    "token",
    "access_token",
    "authorization",
    "value",
    "valuequantity",
    "systolic",
    "diastolic",
    "temperature",
    "temp_c",
    "resource",
    "body",
    "secret",
    "ssn",
}
REDACTED = "[REDACTED]"


def _redact(obj: Any, depth: int = 0) -> Any:
    if depth > 6:
        return obj
    if isinstance(obj, dict):
        return {
            k: (REDACTED if k.lower().replace("-", "_") in PHI_KEYS else _redact(v, depth + 1))
            for k, v in obj.items()
        }
    if isinstance(obj, (list, tuple)):
        return [_redact(v, depth + 1) for v in obj]
    return obj


def redact_phi(_logger: Any, _method: str, event_dict: dict[str, Any]) -> dict[str, Any]:
    event = event_dict.pop("event", None)
    cleaned = _redact(event_dict)
    if event is not None:
        cleaned["event"] = event
    return cleaned


def add_context(_logger: Any, _method: str, event_dict: dict[str, Any]) -> dict[str, Any]:
    rid = request_id_var.get()
    if rid:
        event_dict.setdefault("request_id", rid)
    user = user_var.get()
    if user:
        event_dict.setdefault("user", user)
    return event_dict


def configure_logging(level: str = "INFO", json_output: bool = True) -> None:
    shared = [
        structlog.contextvars.merge_contextvars,
        add_context,
        structlog.stdlib.add_log_level,
        structlog.stdlib.add_logger_name,
        structlog.processors.TimeStamper(fmt="iso", utc=True),
        redact_phi,
        structlog.processors.format_exc_info,
    ]
    renderer = (
        structlog.processors.JSONRenderer() if json_output else structlog.dev.ConsoleRenderer()
    )

    structlog.configure(
        processors=[*shared, structlog.stdlib.ProcessorFormatter.wrap_for_formatter],
        logger_factory=structlog.stdlib.LoggerFactory(),
        cache_logger_on_first_use=False,
    )
    formatter = structlog.stdlib.ProcessorFormatter(
        foreign_pre_chain=shared,
        processors=[structlog.stdlib.ProcessorFormatter.remove_processors_meta, renderer],
    )
    handler = logging.StreamHandler(sys.stdout)
    handler.setFormatter(formatter)
    root = logging.getLogger()
    root.handlers[:] = [handler]
    root.setLevel(level)
    # uvicorn's access log duplicates our request log; keep its errors only.
    logging.getLogger("uvicorn.access").handlers[:] = []
    logging.getLogger("uvicorn.access").propagate = False
    for name in ("uvicorn", "uvicorn.error"):
        logging.getLogger(name).handlers[:] = []
        logging.getLogger(name).propagate = True


def get_logger(name: str) -> structlog.stdlib.BoundLogger:
    return structlog.get_logger(name)
