#!/usr/bin/env python3
"""One-command, cross-platform project setup (macOS / Linux / Windows).

    python scripts/setup.py            # venv + deps + git hooks + local secrets + TLS cert
    python scripts/setup.py --secrets  # only (re)generate deploy/secrets for docker compose

Idempotent: re-running skips anything that already exists.
"""

from __future__ import annotations

import argparse
import json
import os
import secrets
import subprocess
import sys
import venv
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
VENV = ROOT / ".venv"
BIN = VENV / ("Scripts" if os.name == "nt" else "bin")
PY = BIN / ("python.exe" if os.name == "nt" else "python")
SECRETS = ROOT / "deploy" / "secrets"
CERTS = ROOT / "deploy" / "nginx" / "certs"


def step(msg: str) -> None:
    print(f"\n==> {msg}")


def run(*cmd: str | Path) -> None:
    subprocess.run([str(c) for c in cmd], check=True, cwd=ROOT)


def ensure_venv() -> None:
    if sys.version_info < (3, 11):
        sys.exit("Python >= 3.11 required")
    if not PY.exists():
        step("Creating virtualenv .venv")
        venv.create(VENV, with_pip=True)
    step("Installing pinned dependencies (pip cache is reused between runs)")
    run(PY, "-m", "pip", "install", "-q", "--upgrade", "pip")
    run(PY, "-m", "pip", "install", "-q", "-r", "backend/requirements-dev.txt")


def ensure_hooks() -> None:
    if (ROOT / ".git").exists():
        step("Installing git pre-commit hooks")
        run(BIN / "pre-commit", "install")


def _write_secret(name: str, value: str) -> None:
    path = SECRETS / name
    if path.exists():
        print(f"    keep  {path.relative_to(ROOT)}")
        return
    path.write_text(value)
    if os.name != "nt":
        path.chmod(0o600)
    print(f"    wrote {path.relative_to(ROOT)}")


def ensure_secrets() -> None:
    """Secrets for the prod-mode docker stack. Never committed (.gitignore)."""
    step("Generating deployment secrets")
    sys.path.insert(0, str(ROOT / "backend"))
    from cryptography.fernet import Fernet

    import bcrypt

    SECRETS.mkdir(parents=True, exist_ok=True)
    _write_secret("jwt_secrets", secrets.token_urlsafe(48))
    _write_secret("encryption_keys", Fernet.generate_key().decode())
    _write_secret("grafana_admin", secrets.token_urlsafe(16))
    if not (SECRETS / "users.json").exists():
        creds, users = {}, []
        for name, roles, pid in (("admin", ["admin"], None), ("clinician", ["clinician"], None),
                                 ("patient1", ["patient"], "P001"), ("gateway", ["device"], None)):
            pw = secrets.token_urlsafe(12)
            creds[name] = pw
            users.append({"username": name, "roles": roles, "patient_id": pid,
                          "password_hash": bcrypt.hashpw(pw.encode(), bcrypt.gensalt(12)).decode()})
        _write_secret("users.json", json.dumps(users))
        _write_secret("initial_passwords.txt",
                      "\n".join(f"{k}: {v}" for k, v in creds.items()) + "\n")
        print("    initial passwords for the docker stack: deploy/secrets/initial_passwords.txt")


def ensure_cert() -> None:
    """Self-signed cert via `cryptography` (no openssl binary needed -> works on Windows)."""
    if (CERTS / "server.crt").exists():
        return
    step("Creating self-signed TLS certificate for localhost")
    import datetime
    import ipaddress

    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID

    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (x509.CertificateBuilder().subject_name(name).issuer_name(name)
            .public_key(key.public_key()).serial_number(x509.random_serial_number())
            .not_valid_before(now).not_valid_after(now + datetime.timedelta(days=365))
            .add_extension(x509.SubjectAlternativeName([
                x509.DNSName("localhost"), x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]),
                critical=False)
            .sign(key, hashes.SHA256()))
    CERTS.mkdir(parents=True, exist_ok=True)
    (CERTS / "server.key").write_bytes(key.private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
        serialization.NoEncryption()))
    (CERTS / "server.crt").write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    print(f"    wrote {CERTS.relative_to(ROOT)}/server.crt")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--secrets", action="store_true", help="only generate secrets + cert")
    args = ap.parse_args()
    if not args.secrets:
        ensure_venv()
        ensure_hooks()
    if not PY.exists():
        sys.exit("run without --secrets first to create the virtualenv")
    if Path(sys.executable).resolve() != PY.resolve():
        # re-exec inside the venv so cryptography/bcrypt are importable
        run(PY, Path(__file__), "--secrets")
        if not args.secrets:
            print("\nDone. Next:  make dev   (or: cd backend && ../.venv/bin/uvicorn app.main:app)")
        return
    ensure_secrets()
    ensure_cert()


if __name__ == "__main__":
    main()
