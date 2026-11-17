"""Centralised error handling.

All errors leave the API as a FHIR OperationOutcome with a *safe* message plus the
request id. Internal details (stack traces, SQL, file paths) go to the log only.
"""

from __future__ import annotations

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from starlette.exceptions import HTTPException as StarletteHTTPException

from .logging_setup import get_logger, request_id_var

log = get_logger("errors")

FHIR_JSON = "application/fhir+json"


class AppError(Exception):
    status = 400
    code = "processing"

    def __init__(self, message: str, issues: list[dict] | None = None):
        super().__init__(message)
        self.message = message
        self.issues = issues


class ValidationFailed(AppError):
    status = 422
    code = "invalid"


class AuthError(AppError):
    status = 401
    code = "login"


class ForbiddenError(AppError):
    status = 403
    code = "forbidden"


class NotFoundError(AppError):
    status = 404
    code = "not-found"


class RateLimited(AppError):
    status = 429
    code = "throttled"


class ServiceUnavailable(AppError):
    status = 503
    code = "transient"


def outcome_response(status: int, issues: list[dict], headers: dict | None = None) -> JSONResponse:
    from .fhir.builders import operation_outcome

    body = operation_outcome(issues)
    rid = request_id_var.get()
    if rid:
        body["id"] = rid
    return JSONResponse(body, status_code=status, media_type=FHIR_JSON, headers=headers)


def install_handlers(app: FastAPI) -> None:
    @app.exception_handler(AppError)
    async def _app_error(request: Request, exc: AppError):
        level = "warning" if exc.status < 500 else "error"
        getattr(log, level)(
            "request_failed",
            status=exc.status,
            error=exc.code,
            path=request.url.path,
            reason=exc.message,
        )
        issues = exc.issues or [{"severity": "error", "code": exc.code, "diagnostics": exc.message}]
        headers = {"WWW-Authenticate": "Bearer"} if exc.status == 401 else None
        if exc.status == 429:
            headers = {"Retry-After": "60"}
        return outcome_response(exc.status, issues, headers)

    @app.exception_handler(RequestValidationError)
    async def _req_validation(request: Request, exc: RequestValidationError):
        issues = [
            {
                "severity": "error",
                "code": "invalid",
                "diagnostics": str(e.get("msg", "invalid"))[:200],
                "expression": ".".join(str(p) for p in e.get("loc", ())),
            }
            for e in exc.errors()[:20]
        ]
        log.warning("request_invalid", path=request.url.path, issues=len(issues))
        return outcome_response(422, issues)

    @app.exception_handler(StarletteHTTPException)
    async def _http(request: Request, exc: StarletteHTTPException):
        code = {404: "not-found", 405: "not-supported", 413: "too-costly"}.get(
            exc.status_code, "processing"
        )
        return outcome_response(
            exc.status_code, [{"severity": "error", "code": code, "diagnostics": str(exc.detail)}]
        )

    @app.exception_handler(Exception)
    async def _unhandled(request: Request, exc: Exception):
        log.exception("unhandled_exception", path=request.url.path)
        return outcome_response(
            500,
            [
                {
                    "severity": "fatal",
                    "code": "exception",
                    "diagnostics": "Internal server error. Quote request id "
                    f"{request_id_var.get()} to support.",
                }
            ],
        )
