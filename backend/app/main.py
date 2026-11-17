"""FastAPI application factory."""

from __future__ import annotations

from contextlib import asynccontextmanager
from pathlib import Path

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware
from fastapi.staticfiles import StaticFiles

from .api import routes_auth, routes_fhir, routes_ops
from .config import ROOT_DIR, Settings, get_settings
from .errors import install_handlers
from .logging_setup import configure_logging, get_logger
from .security.middleware import BodySizeLimitMiddleware, RequestContextMiddleware
from .services.core import Services

log = get_logger("app")
FRONTEND_DIR = Path(ROOT_DIR / "frontend")


def create_app(settings: Settings | None = None) -> FastAPI:
    settings = settings or get_settings()
    configure_logging(settings.log_level, settings.log_json)

    @asynccontextmanager
    async def lifespan(app: FastAPI):
        svc = Services(settings)
        app.state.services = svc
        await svc.start()
        log.info("startup", env=settings.app_env.value, simulator=settings.simulator_enabled)
        try:
            yield
        finally:
            await svc.stop()
            log.info("shutdown")

    app = FastAPI(
        title=settings.app_name,
        version="1.0.0",
        lifespan=lifespan,
        docs_url="/docs" if settings.docs_enabled else None,
        redoc_url=None,
        openapi_url="/openapi.json" if settings.docs_enabled else None,
    )
    install_handlers(app)

    if settings.cors_origin_list:
        app.add_middleware(
            CORSMiddleware,
            allow_origins=settings.cors_origin_list,
            allow_methods=["GET", "POST"],
            allow_headers=["Authorization", "Content-Type"],
        )
    app.add_middleware(RequestContextMiddleware)
    app.add_middleware(BodySizeLimitMiddleware, max_bytes=settings.max_body_bytes)

    app.include_router(routes_auth.router)
    app.include_router(routes_fhir.router)
    app.include_router(routes_ops.router)

    if settings.serve_frontend and FRONTEND_DIR.is_dir():
        app.mount("/", StaticFiles(directory=FRONTEND_DIR, html=True), name="frontend")
    return app


app = create_app()
