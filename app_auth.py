"""GitHub App authentication: signed JWT -> installation access token.

Uses PyJWT (for RS256 JWT signing) and the standard library for the HTTP call.
The installation token is valid for ~1h; we fetch a fresh one per review
(low-frequency workload, no caching needed).
"""
from __future__ import annotations

import json
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Optional

import jwt  # PyJWT


def _read_key(private_key_path: str) -> str:
    p = Path(private_key_path)
    if not p.is_file():
        raise FileNotFoundError(f"GitHub App private key not found: {p}")
    return p.read_text(encoding="utf-8")


def app_jwt(app_id: str, private_key_path: str) -> str:
    """Build a short-lived RS256 JWT authenticating as the GitHub App itself."""
    pem = _read_key(private_key_path)
    now = int(time.time())
    payload = {
        "iat": now - 60,          # backdate to tolerate clock skew
        "exp": now + 9 * 60,      # GitHub allows up to 10 min
        "iss": app_id,
    }
    return jwt.encode(payload, pem, algorithm="RS256")


def _post_json(url: str, headers: dict, body: dict | None = None) -> dict:
    data = json.dumps(body or {}).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.loads(resp.read().decode("utf-8"))


def installation_token(app_id: str, installation_id: int, private_key_path: str) -> str:
    """Exchange an App JWT for an installation access token (1h TTL)."""
    token = app_jwt(app_id, private_key_path)
    url = f"https://api.github.com/app/installations/{installation_id}/access_tokens"
    headers = {
        "Authorization": f"Bearer {token}",
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
    }
    out = _post_json(url, headers)
    tok = out.get("token")
    if not tok:
        raise RuntimeError(f"GitHub did not return an installation token: {out}")
    return tok


def installation_id_for_owner(app_id: str, private_key_path: str, owner: str) -> Optional[int]:
    """Look up the installation id for a given org/user owner (best-effort)."""
    token = app_jwt(app_id, private_key_path)
    url = f"https://api.github.com/orgs/{owner}/installation"
    req = urllib.request.Request(url, headers={
        "Authorization": f"Bearer {token}",
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
    })
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.loads(resp.read().decode("utf-8")).get("id")
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        raise
