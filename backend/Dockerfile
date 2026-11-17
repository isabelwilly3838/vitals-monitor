# syntax=docker/dockerfile:1.7
# ---- build stage: compile wheels once, cached by requirements hash
FROM python:3.12-slim AS build
ENV PIP_DISABLE_PIP_VERSION_CHECK=1 PIP_NO_CACHE_DIR=0
WORKDIR /build
COPY backend/requirements.txt .
RUN --mount=type=cache,target=/root/.cache/pip \
    pip wheel --wheel-dir /wheels -r requirements.txt

# ---- runtime stage: minimal, non-root, read-only friendly
FROM python:3.12-slim AS runtime
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 APP_ENV=prod
RUN groupadd --system app && useradd --system --gid app --home /app --shell /usr/sbin/nologin app \
    && mkdir -p /data && chown app:app /data
WORKDIR /app
COPY --from=build /wheels /wheels
RUN pip install --no-cache-dir --no-index --find-links=/wheels /wheels/* && rm -rf /wheels
COPY --chown=app:app backend/app ./app
COPY --chown=app:app config /config   # app/config.py resolves ROOT_DIR to / inside the image
USER app
EXPOSE 8000
HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=3 \
  CMD python -c "import urllib.request,sys; sys.exit(0 if urllib.request.urlopen('http://127.0.0.1:8000/health/live',timeout=2).status==200 else 1)"
CMD ["uvicorn", "app.main:app", "--host", "0.0.0.0", "--port", "8000", "--proxy-headers", "--forwarded-allow-ips=*", "--no-server-header"]
