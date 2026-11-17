"""At-rest encryption with key rotation.

Uses Fernet (AES-128-CBC + HMAC-SHA256, authenticated). `MultiFernet` encrypts with the
first key and decrypts with any listed key, so rotation is:
  1. prepend a new key to ENCRYPTION_KEYS and restart
  2. POST /admin/keys/rotate  (re-encrypts every stored row with the new primary key)
  3. drop the old key from ENCRYPTION_KEYS
"""

from __future__ import annotations

from pathlib import Path

from cryptography.fernet import Fernet, InvalidToken, MultiFernet

from ..logging_setup import get_logger

log = get_logger(__name__)


class DecryptionError(Exception):
    pass


class Cipher:
    def __init__(self, keys: list[str]):
        if not keys:
            raise ValueError("at least one encryption key is required")
        self._fernet = MultiFernet([Fernet(k.encode()) for k in keys])
        self.key_count = len(keys)

    def encrypt(self, data: bytes) -> bytes:
        return self._fernet.encrypt(data)

    def decrypt(self, token: bytes) -> bytes:
        try:
            return self._fernet.decrypt(token)
        except InvalidToken as e:
            raise DecryptionError("ciphertext could not be decrypted with any known key") from e

    def rotate(self, token: bytes) -> bytes:
        return self._fernet.rotate(token)


def generate_key() -> str:
    return Fernet.generate_key().decode()


def load_or_create_dev_key(path: Path) -> str:
    """Dev/test convenience only: persist a local key so data survives restarts."""
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        return path.read_text().strip()
    key = generate_key()
    path.write_text(key)
    path.chmod(0o600)
    log.warning("dev_encryption_key_created", path=str(path))
    return key
