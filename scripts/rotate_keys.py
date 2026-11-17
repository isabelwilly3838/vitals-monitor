#!/usr/bin/env python3
"""Rotate the at-rest encryption key and JWT signing secret for the docker stack.

1. prepends a fresh key to deploy/secrets/encryption_keys and jwt_secrets
   (old keys stay listed so existing data/tokens remain valid)
2. after restarting the backend, call POST /admin/keys/rotate to re-encrypt all rows
3. run again with --prune to drop all but the newest key once step 2 succeeded
"""
import argparse
import secrets
from pathlib import Path

from cryptography.fernet import Fernet

S = Path(__file__).resolve().parents[1] / "deploy" / "secrets"


def rotate(name: str, new: str, prune: bool) -> None:
    path = S / name
    keys = [k for k in path.read_text().strip().split(",") if k]
    keys = keys[:1] if prune else [new, *keys][:3]
    path.write_text(",".join(keys))
    print(f"{name}: {len(keys)} key(s) active")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--prune", action="store_true")
    a = ap.parse_args()
    rotate("encryption_keys", Fernet.generate_key().decode(), a.prune)
    rotate("jwt_secrets", secrets.token_urlsafe(48), a.prune)
    print("Now: docker compose restart backend && "
          "curl -k -X POST https://localhost/admin/keys/rotate -H 'Authorization: Bearer <admin>'")
